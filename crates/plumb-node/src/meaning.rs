//! Search by meaning: the embeddings `plumb-tools embed` makes, which
//! `plumb serve`, `plumb-tools search` and `eval` use with `--vectors` and
//! `--model`, and the ones a node keeps with `plumb run --search-by-meaning`.

use plumb_core::locks::RwLocked;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use plumb_core::RecordSet;
use plumb_embed::{
    site_text_words, text_hash, Embedder, ModelId, Vectors, MODEL_FILES, MODEL_NAME,
};
use plumb_index::Meaning;
use tracing::{info, warn};

use crate::cli::{MeaningArgs, QueryInstruction};

/// Sites nearest a query in meaning: closeness is spread over them, and
/// the search ranks the nearest of them and the most popular.
const NEAREST: usize = 1_000;
/// How far the nearest site's cosine must stand above the last of the
/// [`NEAREST`] for closeness to reach 1; below that, closeness shrinks in
/// proportion. The raw cosine can't tell what is near: on hpc's 1,000
/// review searches, a right site found by meaning and an unrelated one
/// leading the results ("trifling glyph of magicka": an Italian theme
/// park) both scored about 0.7. How far the nearest stood out did: by 0.12
/// (median) where an unrelated site led, 0.15 where meaning found the
/// right one.
const LEAST_SPREAD: f32 = 0.15;
/// Articles nearest a query in meaning looked at for the one to list
/// ([`QueryMeaning::nearest_page`]).
const NEAREST_PAGES: usize = 10;
/// How far the nearest article's cosine must stand above the last of the
/// [`NEAREST_PAGES`] for it to be listed. As with sites, the raw cosine
/// can't tell a right article from one nobody asked for. How far the
/// nearest stood out could, somewhat: of 7,902 searches on hpc, this kept
/// 93% of the nearest articles that were the one wanted, 57% of those
/// that were not, and 77% of those for searches that want no article.
const LEAST_PAGE_GAP: f32 = 0.05;
/// Vectors made between saves of the vectors file, so a stopped run keeps
/// most of its work.
pub const SAVE_EVERY: usize = 10_000;
/// Vectors made between progress reports.
const REPORT_EVERY: usize = 1_000;

/// The signed set of the site vectors `model` made (`plumb-tools set
/// vectors`): `plumb/vectors-` and the first 16 hex digits of the model's
/// id, since a set name holds 64 characters at most.
pub fn vectors_set(model: &ModelId) -> String {
    let start: String = model[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("plumb/vectors-{start}")
}

/// A model and the vectors it made, for ranking by meaning. The vectors can
/// grow while searches use them.
pub struct MeaningIndex {
    embedder: Embedder,
    vectors: RwLock<Vectors>,
    /// Vectors of Wikipedia articles by address (`plumb-tools
    /// embed-pages`), when given.
    pages: Option<Vectors>,
    instruction: QueryInstruction,
}

/// What the model was trained to read before a search query.
pub const QUERY_INSTRUCTION: &str = "Represent this sentence for searching relevant passages: ";

impl MeaningIndex {
    /// Loads the model in `model_dir` and the vectors in `vectors`, which
    /// must have been made by that model.
    pub fn open(model_dir: &Path, vectors: &Path) -> Result<Self> {
        let embedder = load_embedder(model_dir)?;
        let vectors = Vectors::load(vectors)?;
        if !made_by(&vectors, &embedder) {
            bail!(
                "the vectors were made by another model than the one in {}; run plumb-tools embed again",
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
            pages: None,
            instruction: QueryInstruction::default(),
        }
    }

    /// Opens the model and vectors `args` name, if it names them.
    pub fn from_args(args: &MeaningArgs) -> Result<Option<Self>> {
        let (Some(model), Some(vectors)) = (&args.model, &args.vectors) else {
            return Ok(None);
        };
        let mut meaning = Self::open(model, vectors)?;
        meaning.instruction = args.query_instruction;
        if let Some(pages) = &args.page_vectors {
            meaning.load_pages(pages)?;
        }
        Ok(Some(meaning))
    }

    /// Takes the article vectors in `path` too, which this model must have
    /// made, so searches list the article nearest them in meaning. Every
    /// search reads the whole file, about 470 bytes an article (230 MB for
    /// the 500,000 most read), so it stays in memory.
    pub fn load_pages(&mut self, path: &Path) -> Result<()> {
        let pages = Vectors::load(path)?;
        if !made_by(&pages, &self.embedder) {
            bail!(
                "the article vectors in {} were made by another model; run plumb-tools embed-pages again",
                path.display()
            );
        }
        info!("loaded {} article vectors", pages.len());
        self.pages = Some(pages);
        Ok(())
    }

    pub fn embedder(&self) -> &Embedder {
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
        self.vectors.read_locked()
    }

    /// How close `query` is in meaning to each site; `None` when the query
    /// cannot be embedded, or is written in what the model cannot read
    /// ([`Embedder::reads`]).
    pub fn query(&self, query: &str) -> Option<QueryMeaning<'_>> {
        if !self.embedder.reads(query) {
            return None;
        }
        let embed = |text: &str| match self.embedder.embed(text) {
            Ok(vector) => Some(vector),
            Err(_) => {
                // Embedding errors may include input text too.
                warn!("could not embed a search query; searching by words only");
                None
            }
        };
        let instructed = || embed(&format!("{QUERY_INSTRUCTION}{query}"));
        let plain = || match self.embedder.embed_query(query) {
            Ok(vector) => Some(vector),
            Err(_) => {
                warn!("could not embed a search query; searching by words only");
                None
            }
        };
        let vectors = self.read();
        let instruction = if self.embedder.takes_query_instruction() {
            self.instruction
        } else {
            QueryInstruction::Off
        };
        match instruction {
            QueryInstruction::Off => Some(vec![plain()?]),
            QueryInstruction::On => Some(vec![instructed()?]),
            QueryInstruction::Mix | QueryInstruction::Min => {
                Some(vec![embed(query)?, instructed()?])
            }
            // Ranked by the instructed vector; placed by the plain one.
            QueryInstruction::Split => Some(vec![instructed()?, embed(query)?]),
        }
        .map(|query| QueryMeaning::new(vectors, self.pages.as_ref(), query, instruction))
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
        self.0.read_locked().clone()
    }

    pub fn set(&self, meaning: Arc<MeaningIndex>) {
        *self.0.write_locked() = Some(meaning);
    }
}

/// A query's vector, against the sites' vectors.
///
/// The model's cosines bunch up (unrelated texts score around 0.5, close
/// ones 0.8), which would leave popularity to decide among them. So
/// closeness is spread over the [`NEAREST`] sites: the nearest gets 1, the
/// last of them 0, and sites farther away 0 too. A thousand, not fewer:
/// with hundreds of thousands of sites, a big one the query describes is
/// often a few hundred places down, behind small sites that repeat the
/// query's words, and must still count as close. When the nearest stands
/// out little from the last of them, nothing is really near, and closeness
/// stays below 1 ([`LEAST_SPREAD`]).
pub struct QueryMeaning<'a> {
    vectors: RwLockReadGuard<'a, Vectors>,
    pages: Option<&'a Vectors>,
    nearest: Vec<String>,
    /// The query's vectors (one, or as it is and after the instruction)
    /// with the cosines of the nearest and of the last of the nearest sites.
    spreads: Vec<Spread>,
    /// How the closeness of several vectors is combined.
    combine: QueryInstruction,
}

struct Spread {
    vector: Vec<i8>,
    best: f32,
    floor: f32,
}

impl Spread {
    /// `cosine` spread from the last of the nearest (0) to the nearest
    /// (1), over at least [`LEAST_SPREAD`].
    fn stretch(&self, cosine: f32) -> f32 {
        if self.best > self.floor {
            (cosine - self.floor) / (self.best - self.floor).max(LEAST_SPREAD)
        } else {
            cosine
        }
        .clamp(0.0, 1.0)
    }
}

impl<'a> QueryMeaning<'a> {
    /// The nearest sites are those nearest the first of `vectors`.
    fn new(
        vectors: RwLockReadGuard<'a, Vectors>,
        pages: Option<&'a Vectors>,
        query: Vec<Vec<i8>>,
        combine: QueryInstruction,
    ) -> Self {
        let mut nearest = None;
        let spreads = query
            .into_iter()
            .map(|vector| {
                let found = vectors.nearest(&vector, NEAREST);
                let best = found.first().map_or(0.0, |&(_, cosine)| cosine);
                let floor = match found.last() {
                    // Too few sites to spread: keep the cosines as they are.
                    Some(&(_, cosine)) if found.len() == NEAREST => cosine,
                    _ => 0.0,
                };
                nearest.get_or_insert_with(|| {
                    found
                        .into_iter()
                        .map(|(domain, _)| domain.to_string())
                        .collect()
                });
                Spread {
                    vector,
                    best,
                    floor,
                }
            })
            .collect();
        QueryMeaning {
            vectors,
            pages,
            nearest: nearest.unwrap_or_default(),
            spreads,
            combine,
        }
    }

    fn spread(&self, spread: &Spread, domain: &str) -> Option<f32> {
        Some(spread.stretch(self.vectors.closeness(&spread.vector, domain)?))
    }
}

impl Meaning for QueryMeaning<'_> {
    fn nearest(&self) -> Vec<String> {
        self.nearest.clone()
    }

    /// By the first of the query's vectors, as the nearest sites are:
    /// after the instruction, as nodes embed searches.
    fn nearest_page(&self) -> Option<String> {
        let found = self
            .pages?
            .nearest(&self.spreads.first()?.vector, NEAREST_PAGES);
        match (found.first(), found.last()) {
            (Some(&(url, best)), Some(&(_, last)))
                if found.len() == NEAREST_PAGES && best - last >= LEAST_PAGE_GAP =>
            {
                Some(url.to_string())
            }
            _ => None,
        }
    }

    fn plain_closeness(&self, domain: &str) -> Option<Option<f32>> {
        match self.combine {
            QueryInstruction::Split => Some(self.spread(self.spreads.get(1)?, domain)),
            _ => None,
        }
    }

    fn closeness(&self, domain: &str) -> Option<f32> {
        if self.combine == QueryInstruction::Split {
            return self.spread(self.spreads.first()?, domain);
        }
        let all = self
            .spreads
            .iter()
            .map(|spread| self.spread(spread, domain))
            .collect::<Option<Vec<f32>>>()?;
        match self.combine {
            QueryInstruction::Min => all.into_iter().reduce(f32::min),
            _ => Some(all.iter().sum::<f32>() / all.len().max(1) as f32),
        }
    }
}

/// Loads the model in `dir`.
pub fn load_embedder(dir: &Path) -> Result<Embedder> {
    Embedder::load(dir).with_context(|| format!("loading the model in {}", dir.display()))
}

fn made_by(vectors: &Vectors, embedder: &Embedder) -> bool {
    vectors.model() == embedder.id() && vectors.dim() == embedder.dim()
}

/// The vectors saved in `path` when `embedder` made them, else none.
pub fn load_vectors_for(path: &Path, embedder: &Embedder) -> Result<Vectors> {
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
pub struct Embedded {
    /// Sites embedded.
    pub done: usize,
    /// Sites whose text could not be embedded.
    pub failed: usize,
}

/// Makes a vector for each of `records` whose text changed since its
/// vector was made, best [`plumb_core::SiteRecord::link_score`] first, on `threads`
/// threads, and drops the vectors of sites not in `records`. Calls `save`
/// after every [`SAVE_EVERY`] sites and at the end, and logs progress every
/// [`REPORT_EVERY`], when it also tells `progress` how many of how many
/// are done. Stops early, after a save, once `stop` says so.
///
/// The records are taken, and each is freed once its text is read: a run
/// can take hours, and holding every record that long, next to the copy an
/// index build or crawl loads, is more than a small server has. Only the
/// domain, hash and text of the sites to embed are kept.
pub fn embed_records(
    embedder: &Embedder,
    vectors: &RwLock<Vectors>,
    records: RecordSet,
    threads: usize,
    stop: &(dyn Fn() -> bool + Sync),
    save: &mut dyn FnMut(&RwLock<Vectors>) -> Result<()>,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<Embedded> {
    let todo = sites_to_embed(vectors, records, embedder.text_words());
    embed_sites(embedder, vectors, todo, threads, stop, save, progress)
}

/// A text [`embed_sites`] makes a vector for: its popularity, its key (a
/// site's domain, or a page's URL for `plumb-tools embed-pages`), the
/// hash of its text and the text.
pub type ToEmbed = (f32, String, plumb_embed::TextHash, String);

/// The first half of [`embed_records`]: drops the vectors of sites not in
/// `records`, or with no text there, and returns the sites to embed, best
/// first. The records are freed by the time it returns.
pub(crate) fn sites_to_embed(
    vectors: &RwLock<Vectors>,
    records: RecordSet,
    words: usize,
) -> Vec<ToEmbed> {
    let total = records.len();
    let mut domains: HashSet<u64> = HashSet::new();
    let mut todo = Vec::new();
    {
        let vectors = vectors.read_locked();
        for record in records {
            let text = site_text_words(&record, words);
            if text.is_empty() {
                continue;
            }
            domains.insert(domain_hash(&record.domain));
            let hash = text_hash(&text);
            if vectors.get(&record.domain).map(|(saved, _)| saved) != Some(&hash) {
                let score = record.link_score();
                todo.push((score, record.domain, hash, text));
            }
        }
    }
    vectors
        .write_locked()
        .retain(|domain| domains.contains(&domain_hash(domain)));
    // The most popular sites first: a long run (all of them, after
    // their text changed) gets to the sites most searched for early.
    todo.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    todo.shrink_to_fit();
    info!(
        "{} of {total} sites need a vector ({} have one)",
        todo.len(),
        vectors.read_locked().len()
    );
    todo
}

/// [`sites_to_embed`] for a node, reading the records file at `path` a
/// record at a time (its journal folded in first, see [`crate::outline`])
/// rather than holding every record: drops the vectors of sites not in the
/// file, or with no text there (a homepage now read as a placeholder,
/// [`plumb_core::is_stand_in_homepage`]), and returns the best `max` sites to
/// embed, best first, and whether
/// more wait for a vector. The text of each site to embed is held until it
/// is embedded, so a node takes them `max` at a time.
pub(crate) fn sites_to_embed_from_file(
    vectors: &RwLock<Vectors>,
    path: &Path,
    max: usize,
    words: usize,
    budget: Option<Arc<plumb_core::storage::StorageBudget>>,
) -> Result<(Vec<ToEmbed>, bool)> {
    let max = max.max(1);
    crate::records::RecordStore::open(path)
        .with_budget(budget)
        .fold()?;
    let mut domains: HashSet<u64> = HashSet::new();
    let mut todo: Vec<ToEmbed> = Vec::new();
    let mut more = false;
    let mut total = 0usize;
    let best_first = |a: &ToEmbed, b: &ToEmbed| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1));
    {
        let held = vectors.read_locked();
        crate::outline::for_each_record(path, |record| {
            total += 1;
            let text = site_text_words(&record, words);
            if text.is_empty() {
                return;
            }
            domains.insert(domain_hash(&record.domain));
            let hash = text_hash(&text);
            if held.get(&record.domain).map(|(saved, _)| saved) != Some(&hash) {
                todo.push((record.link_score(), record.domain, hash, text));
                if todo.len() >= 2 * max {
                    todo.select_nth_unstable_by(max, best_first);
                    todo.truncate(max);
                    more = true;
                }
            }
        })?;
    }
    if todo.len() > max {
        todo.select_nth_unstable_by(max, best_first);
        todo.truncate(max);
        more = true;
    }
    vectors
        .write_locked()
        .retain(|domain| domains.contains(&domain_hash(domain)));
    // The most popular sites first, as sites_to_embed orders them.
    todo.sort_by(best_first);
    todo.shrink_to_fit();
    info!(
        "{}{} of {total} sites need a vector ({} have one)",
        if more { "the best " } else { "" },
        todo.len(),
        vectors.read_locked().len()
    );
    Ok((todo, more))
}

/// The sites of the records file at `path` whose vector is missing or
/// made from other text than theirs now, each with the hash of its text:
/// the vectors another node may give this one.
pub(crate) fn wanted_texts(
    vectors: &RwLock<Vectors>,
    path: &Path,
    words: usize,
    budget: Option<Arc<plumb_core::storage::StorageBudget>>,
) -> Result<std::collections::HashMap<String, plumb_embed::TextHash>> {
    crate::records::RecordStore::open(path)
        .with_budget(budget)
        .fold()?;
    let mut wanted = std::collections::HashMap::new();
    let held = vectors.read_locked();
    crate::outline::for_each_record(path, |record| {
        let text = site_text_words(&record, words);
        if text.is_empty() {
            return;
        }
        let hash = text_hash(&text);
        if held.get(&record.domain).map(|(saved, _)| saved) != Some(&hash) {
            wanted.insert(record.domain, hash);
        }
    })?;
    Ok(wanted)
}

/// A 64-bit hash of `domain`, the same in every run.
fn domain_hash(domain: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    domain.hash(&mut hasher);
    hasher.finish()
}

/// The second half of [`embed_records`]: embeds `todo`, each under its
/// key.
pub fn embed_sites(
    embedder: &Embedder,
    vectors: &RwLock<Vectors>,
    todo: Vec<ToEmbed>,
    threads: usize,
    stop: &(dyn Fn() -> bool + Sync),
    save: &mut dyn FnMut(&RwLock<Vectors>) -> Result<()>,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<Embedded> {
    let write = || vectors.write_locked();
    let started = Instant::now();
    let done = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let chunks = todo.len().div_ceil(REPORT_EVERY);
    progress(0, todo.len());
    for (n, chunk) in todo.chunks(REPORT_EVERY).enumerate() {
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads.max(1) {
                scope.spawn(|| {
                    crate::lower_thread_priority();
                    loop {
                        if stop() {
                            break;
                        }
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some((_, domain, hash, text)) = chunk.get(i) else {
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
                    }
                });
            }
        });
        let last = n + 1 == chunks || stop();
        if last || (n + 1) % (SAVE_EVERY / REPORT_EVERY) == 0 {
            save(vectors)?;
        }
        let done = done.load(Ordering::Relaxed);
        progress(done, todo.len());
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
        save(vectors)?;
    }
    Ok(Embedded {
        done: done.into_inner(),
        failed: failed.into_inner(),
    })
}

/// The model search by meaning runs on a node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum MeaningModel {
    /// bge-small-en-v1.5 (about 130 MB, English).
    #[default]
    Small,
    /// EmbeddingGemma 2 at 256 values (about 310 MB, many languages).
    Gemma,
}

impl MeaningModel {
    /// The directory of its files in a node's data directory.
    pub fn dir_name(self) -> &'static str {
        match self {
            MeaningModel::Small => "model",
            MeaningModel::Gemma => "model-gemma",
        }
    }

    /// About how big its files are, in MB.
    pub fn size_mb(self) -> u64 {
        match self {
            MeaningModel::Small => 130,
            MeaningModel::Gemma => 320,
        }
    }
}

/// Downloads EmbeddingGemma's files into `dir` from `downloads` (each file
/// name with its address), those not there yet.
pub(crate) async fn ensure_gemma_with_budget(
    dir: &Path,
    downloads: &[(String, String)],
    budget: Option<Arc<plumb_core::storage::StorageBudget>>,
) -> Result<()> {
    if downloads.iter().all(|(name, _)| dir.join(name).is_file()) {
        return Ok(());
    }
    plumb_core::storage::create_directory(dir, budget.as_ref())
        .with_context(|| format!("creating {}", dir.display()))?;
    let client = plumb_ingest::download::http_client()?;
    info!("downloading the embedding model EmbeddingGemma 2");
    for (name, url) in downloads {
        let dest = dir.join(name);
        if dest.is_file() {
            continue;
        }
        download_file_with_budget(&client, url, &dest, budget.clone())
            .await
            .with_context(|| format!("downloading {url}"))?;
    }
    Ok(())
}

/// Downloads the model's files into `dir` from `base_url` (each of
/// [`MODEL_FILES`] appended), those not there yet; nothing when `dir`
/// names an embedding server ([`plumb_embed::SERVER_FILE`]).
pub async fn ensure_model(dir: &Path, base_url: &str) -> Result<()> {
    ensure_model_with_budget(dir, base_url, None).await
}

pub(crate) async fn ensure_model_with_budget(
    dir: &Path,
    base_url: &str,
    budget: Option<Arc<plumb_core::storage::StorageBudget>>,
) -> Result<()> {
    if MODEL_FILES.iter().all(|name| dir.join(name).is_file())
        || dir.join(plumb_embed::SERVER_FILE).is_file()
    {
        return Ok(());
    }
    plumb_core::storage::create_directory(dir, budget.as_ref())
        .with_context(|| format!("creating {}", dir.display()))?;
    let client = plumb_ingest::download::http_client()?;
    info!("downloading the embedding model {MODEL_NAME}");
    for name in MODEL_FILES {
        let dest: PathBuf = dir.join(name);
        if dest.is_file() {
            continue;
        }
        let url = format!("{base_url}{name}");
        download_file_with_budget(&client, &url, &dest, budget.clone())
            .await
            .with_context(|| format!("downloading {url}"))?;
    }
    Ok(())
}

// Keep the downloader's resumable policy while scoping every staged write,
// sidecar, cleanup and replacement to this node's shared ledger.
pub(crate) async fn download_file_with_budget(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    budget: Option<Arc<plumb_core::storage::StorageBudget>>,
) -> Result<u64> {
    plumb_ingest::storage::with_budget(
        budget,
        plumb_ingest::download::download_to_file(client, url, dest),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use plumb_core::SiteRecord;

    fn record(domain: &str, title: &str) -> SiteRecord {
        let mut record = SiteRecord::new(domain);
        record.title = Some(title.into());
        record
    }

    #[test]
    fn the_most_popular_sites_are_embedded_first() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("model");
        plumb_embed::write_test_model(&model).unwrap();
        let embedder = Embedder::load(&model).unwrap();
        let vectors = RwLock::new(Vectors::new(embedder.id(), embedder.dim()));
        let mut obscure = record("aaa.example", "Some small site");
        obscure.signals.tranco_rank = Some(900_000);
        let mut popular = record("zzz.example", "A big site");
        popular.signals.tranco_rank = Some(10);
        // Stops after the first site.
        let asked = AtomicUsize::new(0);
        let stop = || asked.fetch_add(1, Ordering::Relaxed) >= 1;
        embed_records(
            &embedder,
            &vectors,
            [obscure, popular].into_iter().collect(),
            1,
            &stop,
            &mut |_| Ok(()),
            &mut |_, _| {},
        )
        .unwrap();
        let vectors = vectors.into_inner().unwrap();
        assert_eq!(vectors.len(), 1);
        assert!(vectors.get("zzz.example").is_some());
    }

    #[test]
    fn closeness_reaches_one_only_when_the_nearest_stands_out() {
        let spread = |best, floor| Spread {
            vector: Vec::new(),
            best,
            floor,
        };
        // hpc's "trifling glyph of magicka": the nearest stands out by 0.09,
        // and an Italian theme park at 0.659 no longer counts as close.
        let nothing_near = spread(0.702, 0.611);
        assert!((nothing_near.stretch(0.659) - 0.32).abs() < 0.01);
        assert!((nothing_near.stretch(0.702) - 0.61).abs() < 0.01);
        // "paris saint-germain": psg.fr stands out by 0.16 and stays at 1.
        let near = spread(0.794, 0.634);
        assert_eq!(near.stretch(0.794), 1.0);
        assert!((near.stretch(0.714) - 0.5).abs() < 0.01);
        assert_eq!(near.stretch(0.6), 0.0);
    }

    #[test]
    fn the_nearest_article_is_listed_only_when_it_stands_out() {
        let model = [0; 32];
        let sites = RwLock::new(Vectors::new(model, 2));
        // Ten articles at these cosines to a query pointing along x.
        let articles = |cosines: &[f32]| {
            let mut pages = Vectors::new(model, 2);
            for (n, cosine) in cosines.iter().enumerate() {
                let sine = (1.0 - cosine * cosine).sqrt();
                let vector = [(cosine * 127.0) as i8, (sine * 127.0) as i8];
                let url = format!("https://en.wikipedia.org/wiki/{n}");
                pages.insert(&url, text_hash(&url), &vector).unwrap();
            }
            pages
        };
        let nearest = |pages: &Vectors| {
            QueryMeaning::new(
                sites.read_locked(),
                Some(pages),
                vec![vec![127, 0]],
                QueryInstruction::Split,
            )
            .nearest_page()
        };
        let stands_out = articles(&[1.0, 0.95, 0.94, 0.94, 0.93, 0.93, 0.92, 0.92, 0.91, 0.9]);
        assert_eq!(
            nearest(&stands_out).as_deref(),
            Some("https://en.wikipedia.org/wiki/0")
        );
        let bunched = articles(&[1.0, 0.99, 0.99, 0.98, 0.98, 0.98, 0.98, 0.97, 0.97, 0.97]);
        assert_eq!(nearest(&bunched), None);
        assert_eq!(nearest(&articles(&[1.0, 0.5])), None, "too few to tell");
    }

    #[test]
    fn a_site_left_with_no_text_loses_its_vector() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("model");
        plumb_embed::write_test_model(&model).unwrap();
        let embedder = Embedder::load(&model).unwrap();
        let vectors = RwLock::new(Vectors::new(embedder.id(), embedder.dim()));
        let embed = |text: &str| {
            let mut parked = record("parked.example", text);
            parked.body_text = Some(text.into());
            let records = [parked].into_iter().collect();
            embed_records(
                &embedder,
                &vectors,
                records,
                1,
                &|| false,
                &mut |_| Ok(()),
                &mut |_, _| {},
            )
            .unwrap();
        };
        embed("Parked Things");
        assert!(vectors.read_locked().get("parked.example").is_some());
        // Crawled again, its homepage only says hello: a placeholder.
        embed("Hello!");
        assert!(vectors.read_locked().get("parked.example").is_none());
    }
}
