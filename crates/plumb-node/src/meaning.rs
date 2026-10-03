//! Search by meaning: `plumb embed`, and the embeddings `search`, `serve`
//! and `eval` use with `--vectors` and `--model`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use plumb_embed::{
    site_text, text_hash, Embedder, Vectors, MODEL_BASE_URL, MODEL_FILES, MODEL_NAME,
};
use plumb_index::Meaning;
use tracing::{info, warn};

use crate::block_on;
use crate::cli::{EmbedArgs, MeaningArgs};
use crate::records::load_records;

/// Sites nearest a query in meaning that are ranked with the rest.
const NEAREST: usize = 50;
/// Vectors made between saves of the vectors file, so a stopped run keeps
/// most of its work.
const SAVE_EVERY: usize = 10_000;

/// A model and the vectors it made, for ranking by meaning.
pub struct MeaningIndex {
    embedder: Embedder,
    vectors: Vectors,
}

impl MeaningIndex {
    /// Loads the model in `model_dir` and the vectors in `vectors`, which
    /// must have been made by that model.
    pub fn open(model_dir: &Path, vectors: &Path) -> Result<Self> {
        let embedder = Embedder::load(model_dir)
            .with_context(|| format!("loading the model in {}", model_dir.display()))?;
        let vectors = Vectors::load(vectors)?;
        if vectors.model() != embedder.id() || vectors.dim() != embedder.dim() {
            bail!(
                "the vectors were made by another model than the one in {}; run plumb embed again",
                model_dir.display()
            );
        }
        info!("loaded {} site vectors", vectors.len());
        Ok(MeaningIndex { embedder, vectors })
    }

    /// Opens the model and vectors `args` name, if it names them.
    pub fn from_args(args: &MeaningArgs) -> Result<Option<Self>> {
        match (&args.model, &args.vectors) {
            (Some(model), Some(vectors)) => Self::open(model, vectors).map(Some),
            _ => Ok(None),
        }
    }

    /// How close `query` is in meaning to each site; `None` when the query
    /// cannot be embedded.
    pub fn query(&self, query: &str) -> Option<QueryMeaning<'_>> {
        match self.embedder.embed(query) {
            Ok(vector) => Some(QueryMeaning {
                vectors: &self.vectors,
                vector,
            }),
            Err(err) => {
                warn!("could not embed {query:?}, searching by words only: {err:#}");
                None
            }
        }
    }
}

/// A query's vector, against the sites' vectors.
pub struct QueryMeaning<'a> {
    vectors: &'a Vectors,
    vector: Vec<i8>,
}

impl Meaning for QueryMeaning<'_> {
    fn nearest(&self) -> Vec<String> {
        self.vectors
            .nearest(&self.vector, NEAREST)
            .into_iter()
            .map(|(domain, _)| domain.to_string())
            .collect()
    }

    fn closeness(&self, domain: &str) -> Option<f32> {
        self.vectors.closeness(&self.vector, domain)
    }
}

/// `plumb embed`: downloads the model when missing, then makes a vector
/// for every site in the records file whose text changed since the last
/// run, and drops the vectors of sites no longer there.
pub fn run_embed(args: EmbedArgs) -> Result<()> {
    if MODEL_FILES
        .iter()
        .any(|name| !args.model.join(name).is_file())
    {
        block_on(download_model(&args.model))??;
    }
    let embedder = Embedder::load(&args.model)
        .with_context(|| format!("loading the model in {}", args.model.display()))?;
    let records = load_records(&args.records)
        .with_context(|| format!("loading records {}", args.records.display()))?
        .into_sorted_vec();

    let mut vectors = match Vectors::load(&args.vectors) {
        Ok(vectors) if vectors.model() == embedder.id() && vectors.dim() == embedder.dim() => {
            vectors
        }
        Ok(_) => {
            info!("the saved vectors are from another model; making them all again");
            Vectors::new(embedder.id(), embedder.dim())
        }
        Err(_) if !args.vectors.exists() => Vectors::new(embedder.id(), embedder.dim()),
        Err(err) => return Err(err),
    };
    let domains: std::collections::HashSet<&str> =
        records.iter().map(|r| r.domain.as_str()).collect();
    vectors.retain(|domain| domains.contains(domain));

    let mut todo = Vec::new();
    for record in &records {
        let text = site_text(record);
        if text.is_empty() {
            continue;
        }
        let hash = text_hash(&text);
        if vectors.get(&record.domain).map(|(saved, _)| saved) != Some(&hash) {
            todo.push((record.domain.as_str(), hash, text));
        }
    }
    info!(
        "{} of {} sites need a vector ({} already have one)",
        todo.len(),
        records.len(),
        vectors.len()
    );

    let started = Instant::now();
    let threads = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let vectors = Mutex::new(vectors);
    let done = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    for chunk in todo.chunks(SAVE_EVERY) {
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((domain, hash, text)) = chunk.get(i) else {
                        break;
                    };
                    match embedder.embed(text) {
                        Ok(vector) => {
                            // The vector has the model's length.
                            let _ = vectors.lock().unwrap().insert(domain, *hash, &vector);
                        }
                        Err(err) => {
                            warn!("could not embed the text of {domain}: {err:#}");
                            failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    done.fetch_add(1, Ordering::Relaxed);
                });
            }
        });
        vectors.lock().unwrap().save(&args.vectors)?;
        let done = done.load(Ordering::Relaxed);
        let seconds = started.elapsed().as_secs_f64();
        info!(
            "embedded {done} of {} sites ({:.0} a second)",
            todo.len(),
            done as f64 / seconds.max(0.001)
        );
    }
    let vectors = vectors.into_inner().unwrap();
    vectors.save(&args.vectors)?;
    println!(
        "{} site vectors in {} ({} failed)",
        vectors.len(),
        args.vectors.display(),
        failed.load(Ordering::Relaxed)
    );
    Ok(())
}

/// Downloads the model's files into `dir`.
async fn download_model(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let client = plumb_ingest::download::http_client()?;
    info!("downloading the embedding model {MODEL_NAME}");
    for name in MODEL_FILES {
        let dest: PathBuf = dir.join(name);
        if dest.is_file() {
            continue;
        }
        let url = format!("{MODEL_BASE_URL}{name}");
        plumb_ingest::download::download_to_file(&client, &url, &dest)
            .await
            .with_context(|| format!("downloading {url}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use plumb_core::{write_jsonl, SiteRecord};
    use plumb_index::{build_index, RankConfig, SearchOptions, Searcher};

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
        run_embed(args).unwrap();
        let first = Vectors::load(&vectors_path).unwrap();
        assert_eq!(first.len(), 2, "a site with no text gets no vector");
        let tesla = first.get("tesla.com").unwrap().1.to_vec();

        // A changed site is embedded again; a gone one is dropped.
        records[1].title = Some("American airline flights".into());
        records.remove(0);
        write_jsonl(&records_path, &records).unwrap();
        run_embed(EmbedArgs {
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
                .embed(&site_text(&record(
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
