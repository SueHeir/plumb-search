//! Search by meaning: `plumb embed`, the embeddings `search`, `serve` and
//! `eval` use with `--vectors` and `--model`, and the ones a node keeps with
//! `plumb run --search-by-meaning`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use plumb_core::SiteRecord;
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
pub(crate) const SAVE_EVERY: usize = 10_000;
/// Vectors made between progress reports.
const REPORT_EVERY: usize = 1_000;

/// A model and the vectors it made, for ranking by meaning. The vectors can
/// grow while searches use them.
pub struct MeaningIndex {
    embedder: Embedder,
    vectors: RwLock<Vectors>,
}

impl MeaningIndex {
    /// Loads the model in `model_dir` and the vectors in `vectors`, which
    /// must have been made by that model.
    pub fn open(model_dir: &Path, vectors: &Path) -> Result<Self> {
        let embedder = load_embedder(model_dir)?;
        let vectors = Vectors::load(vectors)?;
        if !made_by(&vectors, &embedder) {
            bail!(
                "the vectors were made by another model than the one in {}; run plumb embed again",
                model_dir.display()
            );
        }
        info!("loaded {} site vectors", vectors.len());
        Ok(Self::new(embedder, vectors))
    }

    pub(crate) fn new(embedder: Embedder, vectors: Vectors) -> Self {
        MeaningIndex {
            embedder,
            vectors: RwLock::new(vectors),
        }
    }

    /// Opens the model and vectors `args` name, if it names them.
    pub fn from_args(args: &MeaningArgs) -> Result<Option<Self>> {
        match (&args.model, &args.vectors) {
            (Some(model), Some(vectors)) => Self::open(model, vectors).map(Some),
            _ => Ok(None),
        }
    }

    pub(crate) fn embedder(&self) -> &Embedder {
        &self.embedder
    }

    pub(crate) fn vectors(&self) -> &RwLock<Vectors> {
        &self.vectors
    }

    /// Number of sites with a vector.
    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read(&self) -> RwLockReadGuard<'_, Vectors> {
        self.vectors.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// How close `query` is in meaning to each site; `None` when the query
    /// cannot be embedded.
    pub fn query(&self, query: &str) -> Option<QueryMeaning<'_>> {
        match self.embedder.embed(query) {
            Ok(vector) => Some(QueryMeaning::new(self.read(), vector)),
            Err(err) => {
                warn!("could not embed {query:?}, searching by words only: {err:#}");
                None
            }
        }
    }
}

/// The [`MeaningIndex`] searches use, if any, which a node sets once its
/// model is loaded.
#[derive(Clone, Default)]
pub struct SharedMeaning(Arc<RwLock<Option<Arc<MeaningIndex>>>>);

impl SharedMeaning {
    pub fn new(meaning: Option<MeaningIndex>) -> Self {
        SharedMeaning(Arc::new(RwLock::new(meaning.map(Arc::new))))
    }

    pub fn get(&self) -> Option<Arc<MeaningIndex>> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set(&self, meaning: Arc<MeaningIndex>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Some(meaning);
    }
}

/// A query's vector, against the sites' vectors.
///
/// The model's cosines bunch up (unrelated texts score around 0.5, close
/// ones 0.8), which would leave popularity to decide among them. So
/// closeness is spread over the [`NEAREST`] sites: the nearest gets 1, the
/// last of them 0, and sites farther away 0 too.
pub struct QueryMeaning<'a> {
    vectors: RwLockReadGuard<'a, Vectors>,
    vector: Vec<i8>,
    nearest: Vec<String>,
    /// Cosines of the nearest and of the last of the nearest sites.
    best: f32,
    floor: f32,
}

impl<'a> QueryMeaning<'a> {
    fn new(vectors: RwLockReadGuard<'a, Vectors>, vector: Vec<i8>) -> Self {
        let found = vectors.nearest(&vector, NEAREST);
        let best = found.first().map_or(0.0, |&(_, cosine)| cosine);
        let floor = match found.last() {
            // Too few sites to spread: keep the cosines as they are.
            Some(&(_, cosine)) if found.len() == NEAREST => cosine,
            _ => 0.0,
        };
        let nearest = found
            .into_iter()
            .map(|(domain, _)| domain.to_string())
            .collect();
        QueryMeaning {
            vectors,
            vector,
            nearest,
            best,
            floor,
        }
    }
}

impl Meaning for QueryMeaning<'_> {
    fn nearest(&self) -> Vec<String> {
        self.nearest.clone()
    }

    fn closeness(&self, domain: &str) -> Option<f32> {
        let cosine = self.vectors.closeness(&self.vector, domain)?;
        if self.best > self.floor {
            Some(((cosine - self.floor) / (self.best - self.floor)).clamp(0.0, 1.0))
        } else {
            Some(cosine.clamp(0.0, 1.0))
        }
    }
}

/// Loads the model in `dir`.
pub(crate) fn load_embedder(dir: &Path) -> Result<Embedder> {
    Embedder::load(dir).with_context(|| format!("loading the model in {}", dir.display()))
}

fn made_by(vectors: &Vectors, embedder: &Embedder) -> bool {
    vectors.model() == embedder.id() && vectors.dim() == embedder.dim()
}

/// The vectors saved in `path` when `embedder` made them, else none.
pub(crate) fn load_vectors_for(path: &Path, embedder: &Embedder) -> Result<Vectors> {
    match Vectors::load(path) {
        Ok(vectors) if made_by(&vectors, embedder) => Ok(vectors),
        Ok(_) => {
            info!("the saved vectors are from another model; making them all again");
            Ok(Vectors::new(embedder.id(), embedder.dim()))
        }
        Err(_) if !path.exists() => Ok(Vectors::new(embedder.id(), embedder.dim())),
        Err(err) => Err(err),
    }
}

/// What [`embed_records`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Embedded {
    /// Sites embedded.
    pub done: usize,
    /// Sites whose text could not be embedded.
    pub failed: usize,
}

/// Makes a vector for each of `records` (best first, as given) whose text
/// changed since its vector was made, on `threads` threads, and drops the
/// vectors of sites not in `records`. Calls `save` after every
/// [`SAVE_EVERY`] sites and at the end, and logs progress every
/// [`REPORT_EVERY`]. Stops early, after a save, once
/// `stop` says so.
pub(crate) fn embed_records(
    embedder: &Embedder,
    vectors: &RwLock<Vectors>,
    records: &[SiteRecord],
    threads: usize,
    stop: &(dyn Fn() -> bool + Sync),
    save: &mut dyn FnMut(&Vectors) -> Result<()>,
) -> Result<Embedded> {
    let write = || vectors.write().unwrap_or_else(PoisonError::into_inner);
    let domains: HashSet<&str> = records.iter().map(|r| r.domain.as_str()).collect();
    write().retain(|domain| domains.contains(domain));

    let mut todo = Vec::new();
    {
        let vectors = vectors.read().unwrap_or_else(PoisonError::into_inner);
        for record in records {
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
            "{} of {} sites need a vector ({} have one)",
            todo.len(),
            records.len(),
            vectors.len()
        );
    }

    let started = Instant::now();
    let done = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let chunks = todo.len().div_ceil(REPORT_EVERY);
    for (n, chunk) in todo.chunks(REPORT_EVERY).enumerate() {
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads.max(1) {
                scope.spawn(|| loop {
                    if stop() {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((domain, hash, text)) = chunk.get(i) else {
                        break;
                    };
                    match embedder.embed(text) {
                        // The vector has the model's length.
                        Ok(vector) => drop(write().insert(domain, *hash, &vector)),
                        Err(err) => {
                            warn!("could not embed the text of {domain}: {err:#}");
                            failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    done.fetch_add(1, Ordering::Relaxed);
                });
            }
        });
        let last = n + 1 == chunks || stop();
        if last || (n + 1) % (SAVE_EVERY / REPORT_EVERY) == 0 {
            save(&vectors.read().unwrap_or_else(PoisonError::into_inner))?;
        }
        let done = done.load(Ordering::Relaxed);
        info!(
            "embedded {done} of {} sites ({:.1} a second)",
            todo.len(),
            done as f64 / started.elapsed().as_secs_f64().max(0.001)
        );
        if stop() {
            break;
        }
    }
    if todo.is_empty() {
        save(&vectors.read().unwrap_or_else(PoisonError::into_inner))?;
    }
    Ok(Embedded {
        done: done.into_inner(),
        failed: failed.into_inner(),
    })
}

/// `plumb embed`: downloads the model when missing, then makes a vector
/// for every site in the records file whose text changed since the last
/// run, and drops the vectors of sites no longer there.
pub fn run_embed(args: EmbedArgs) -> Result<()> {
    block_on(ensure_model(&args.model))??;
    let embedder = load_embedder(&args.model)?;
    let records = load_records(&args.records)
        .with_context(|| format!("loading records {}", args.records.display()))?
        .into_sorted_vec();
    let vectors = RwLock::new(load_vectors_for(&args.vectors, &embedder)?);
    let threads = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let embedded = embed_records(
        &embedder,
        &vectors,
        &records,
        threads,
        &|| false,
        &mut |vectors| vectors.save(&args.vectors),
    )?;
    println!(
        "{} site vectors in {} ({} embedded now, {} failed)",
        vectors.read().unwrap_or_else(PoisonError::into_inner).len(),
        args.vectors.display(),
        embedded.done,
        embedded.failed
    );
    Ok(())
}

/// Downloads the model's files into `dir`, those not there yet.
pub(crate) async fn ensure_model(dir: &Path) -> Result<()> {
    if MODEL_FILES.iter().all(|name| dir.join(name).is_file()) {
        return Ok(());
    }
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
