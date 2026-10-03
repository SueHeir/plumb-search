//! Turning a homepage into the same embedding every time.
//!
//! A homepage becomes a vector in three fixed steps:
//!
//! 1. [`page_text`]: the page's title, description, first heading and
//!    visible body text (as [`plumb_crawl::extract_page_meta`] reads them),
//!    in that order, one per line, whitespace collapsed, at most
//!    [`MAX_TEXT_WORDS`] words. The same HTML always gives the same bytes.
//! 2. [`Embedder::embed`]: the text through one pinned model,
//!    BAAI/bge-small-en-v1.5 as ONNX ([`MODEL_FILES`] lists the files and
//!    their SHA-256; any other file is refused). There is no randomness:
//!    ONNX Runtime on the CPU gives the same floats for the same input, with
//!    any number of threads.
//! 3. [`quantize`]: the unit-length vector rounded to one signed byte per
//!    dimension (`round(x * 127)`). The rounding absorbs the last-bit
//!    differences different CPUs can show in floating point, and the vector
//!    takes 384 bytes.
//!
//! [`text_hash`] and [`vector_hash`] give short fingerprints, so a node can
//! store the text hash next to the vector, re-embed only when it changes,
//! and check a vector another node sent by recomputing it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use plumb_core::collapse_whitespace;
use plumb_crawl::PageMeta;
use sha2::{Digest, Sha256};

/// Most words of [`page_text`]; the model reads at most 512 tokens, and
/// this keeps a page well under that.
pub const MAX_TEXT_WORDS: usize = 256;

/// The model's name, as recorded with each vector.
pub const MODEL_NAME: &str = "BAAI/bge-small-en-v1.5 (Xenova ONNX)";

/// Values per vector.
pub const DIMENSIONS: usize = 384;

/// The model files, relative to the model's snapshot folder, and the
/// SHA-256 each must have. Vectors from other files are not comparable, so
/// [`Embedder::new`] refuses them.
pub const MODEL_FILES: &[(&str, &str)] = &[
    (
        "onnx/model.onnx",
        "828e1496d7fabb79cfa4dcd84fa38625c0d3d21da474a00f08db0f559940cf35",
    ),
    (
        "tokenizer.json",
        "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
    ),
    (
        "config.json",
        "fa73f90bf92c8cace1fbcb709626306f2bdbc9ea3e5b5f94b440df9b6aa56350",
    ),
    (
        "special_tokens_map.json",
        "b6d346be366a7d1d48332dbc9fdf3bf8960b5d879522b7799ddba59e76237ee3",
    ),
    (
        "tokenizer_config.json",
        "9261e7d79b44c8195c1cada2b453e55b00aeb81e907a6664974b4d7776172ab3",
    ),
];

/// The text of a page that goes into its embedding: title, description,
/// first heading (unless the title or body text already holds it) and body
/// text, one per line,
/// each with whitespace collapsed, cut to [`MAX_TEXT_WORDS`] words in all.
/// Empty if the page has none of them.
pub fn page_text(meta: &PageMeta) -> String {
    let heading = meta
        .heading
        .as_ref()
        .filter(|heading| Some(*heading) != meta.title.as_ref())
        .filter(|heading| {
            !meta
                .body_text
                .as_ref()
                .is_some_and(|body| body.contains(*heading))
        });
    let mut left = MAX_TEXT_WORDS;
    let mut lines = Vec::new();
    for part in [
        &meta.title,
        &meta.description,
        &heading.cloned(),
        &meta.body_text,
    ]
    .into_iter()
    .flatten()
    {
        if left == 0 {
            break;
        }
        let words: Vec<&str> = part.split_whitespace().take(left).collect();
        if !words.is_empty() {
            left -= words.len();
            lines.push(words.join(" "));
        }
    }
    collapse_lines(&lines.join("\n"))
}

/// Joins runs of blank lines; keeps the line breaks [`page_text`] puts in.
fn collapse_lines(text: &str) -> String {
    text.lines()
        .map(collapse_whitespace)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A unit-length vector rounded to one signed byte per value.
pub fn quantize(vector: &[f32]) -> Vec<i8> {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    let scale = if norm > 0.0 { 127.0 / norm } else { 0.0 };
    vector
        .iter()
        .map(|x| (x * scale).round().clamp(-127.0, 127.0) as i8)
        .collect()
}

/// Cosine similarity of two quantized vectors.
pub fn similarity(a: &[i8], b: &[i8]) -> f32 {
    let dot: i32 = a.iter().zip(b).map(|(&x, &y)| x as i32 * y as i32).sum();
    let norm = |v: &[i8]| {
        v.iter()
            .map(|&x| (x as i32 * x as i32) as f32)
            .sum::<f32>()
            .sqrt()
    };
    let denom = norm(a) * norm(b);
    if denom == 0.0 {
        0.0
    } else {
        dot as f32 / denom
    }
}

/// SHA-256 of the text, in hex.
pub fn text_hash(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

/// SHA-256 of the vector's bytes, in hex.
pub fn vector_hash(vector: &[i8]) -> String {
    let bytes: Vec<u8> = vector.iter().map(|&x| x as u8).collect();
    hex(&Sha256::digest(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The pinned model, ready to embed.
pub struct Embedder {
    model: TextEmbedding,
}

impl Embedder {
    /// Loads the model from `cache_dir`, downloading it from Hugging Face
    /// the first time, with `threads` ONNX Runtime threads (any number gives
    /// the same vectors). Fails if a model file's SHA-256 differs from
    /// [`MODEL_FILES`].
    pub fn new(cache_dir: &Path, threads: usize) -> Result<Self> {
        let options = TextInitOptions::new(EmbeddingModel::BGESmallENV15)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(false)
            .with_intra_threads(threads.max(1));
        let model = TextEmbedding::try_new(options).context("loading the embedding model")?;
        let snapshot = snapshot_dir(cache_dir)?;
        for (file, expected) in MODEL_FILES {
            let actual = file_hash(&snapshot.join(file))?;
            if actual != *expected {
                bail!(
                    "{file} of {MODEL_NAME} has SHA-256 {actual}, not the pinned {expected}; \
                     its vectors would not match other nodes'"
                );
            }
        }
        Ok(Self { model })
    }

    /// The SHA-256 of each model file, as found in `cache_dir`.
    pub fn model_hashes(cache_dir: &Path) -> Result<Vec<(String, String)>> {
        let snapshot = snapshot_dir(cache_dir)?;
        MODEL_FILES
            .iter()
            .map(|(file, _)| Ok((file.to_string(), file_hash(&snapshot.join(file))?)))
            .collect()
    }

    /// Quantized vectors for `texts`, in order.
    pub fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<i8>>> {
        let vectors = self
            .model
            .embed(texts, Some(32))
            .context("running the embedding model")?;
        Ok(vectors.iter().map(|v| quantize(v)).collect())
    }
}

/// The folder holding the model's files under `cache_dir`.
fn snapshot_dir(cache_dir: &Path) -> Result<PathBuf> {
    let snapshots = cache_dir.join("models--Xenova--bge-small-en-v1.5/snapshots");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&snapshots)
        .with_context(|| format!("reading {}", snapshots.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.join("onnx/model.onnx").exists())
        .collect();
    dirs.sort();
    match dirs.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("no model snapshot in {}", snapshots.display()),
        _ => bail!(
            "{} model snapshots in {}; keep one",
            dirs.len(),
            snapshots.display()
        ),
    }
}

fn file_hash(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(hex(&Sha256::digest(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(title: &str, description: &str, heading: &str, body: &str) -> PageMeta {
        let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
        PageMeta {
            title: some(title),
            description: some(description),
            heading: some(heading),
            body_text: some(body),
            ..PageMeta::default()
        }
    }

    #[test]
    fn page_text_is_title_description_heading_body() {
        let text = page_text(&meta(
            "PNC Bank",
            "Checking,   savings and loans.",
            "Personal Banking",
            "Open an account today.",
        ));
        assert_eq!(
            text,
            "PNC Bank\nChecking, savings and loans.\nPersonal Banking\nOpen an account today."
        );
    }

    #[test]
    fn page_text_skips_a_heading_that_repeats_the_title_and_empty_parts() {
        assert_eq!(
            page_text(&meta("Acme", "", "Acme", "Rockets.")),
            "Acme\nRockets."
        );
        assert_eq!(page_text(&PageMeta::default()), "");
    }

    #[test]
    fn page_text_stops_at_the_word_limit() {
        let body: Vec<String> = (0..1000).map(|i| format!("w{i}")).collect();
        let text = page_text(&meta("One two", "", "", &body.join(" ")));
        assert_eq!(text.split_whitespace().count(), MAX_TEXT_WORDS);
        assert!(text.starts_with("One two\nw0 w1"));
    }

    #[test]
    fn same_html_gives_the_same_text_and_hash() {
        let url = url::Url::parse("https://acme.com/").unwrap();
        let html = "<title>Acme</title><nav>Menu</nav><h1>Rockets</h1><p>Since 1949.</p>";
        let first = page_text(&plumb_crawl::extract_page_meta(&url, html));
        assert_eq!(first, "Acme\nRockets Since 1949.");
        for _ in 0..3 {
            let again = page_text(&plumb_crawl::extract_page_meta(&url, html));
            assert_eq!(text_hash(&again), text_hash(&first));
        }
    }

    #[test]
    fn quantize_scales_to_unit_length_and_rounds() {
        assert_eq!(quantize(&[3.0, 4.0]), vec![76, 102]);
        assert_eq!(quantize(&[0.0, 0.0]), vec![0, 0]);
        assert_eq!(quantize(&[-1.0]), vec![-127]);
    }

    #[test]
    fn similarity_of_quantized_vectors() {
        assert!((similarity(&[127, 0], &[127, 0]) - 1.0).abs() < 1e-6);
        assert!(similarity(&[127, 0], &[0, 127]).abs() < 1e-6);
        assert_eq!(similarity(&[0, 0], &[1, 1]), 0.0);
    }

    /// Needs the model (downloaded on first run, ~130 MB) and ONNX Runtime:
    /// `PLUMB_EMBED_CACHE=dir cargo test -p plumb-embed -- --ignored`.
    #[test]
    #[ignore]
    fn same_text_gives_the_same_bytes_with_any_thread_count() {
        let cache =
            std::env::var("PLUMB_EMBED_CACHE").unwrap_or_else(|_| ".fastembed_cache".into());
        let cache = Path::new(&cache);
        let texts: Vec<String> = [
            "Chicago Tribune\nChicago news, sports, weather and business.",
            "PNC Bank\nChecking, savings and loans.",
            "Rust Programming Language\nA language empowering everyone to build reliable software.",
        ]
        .map(String::from)
        .to_vec();
        let one = Embedder::new(cache, 1).unwrap().embed(&texts).unwrap();
        let mut many = Embedder::new(cache, 8).unwrap();
        assert_eq!(many.embed(&texts).unwrap(), one);
        assert_eq!(many.embed(&texts).unwrap(), one);
        // One at a time gives what a batch gives (padding must not leak in).
        for (text, expected) in texts.iter().zip(&one) {
            assert_eq!(
                &many.embed(std::slice::from_ref(text)).unwrap()[0],
                expected
            );
        }
        assert_eq!(one[0].len(), DIMENSIONS);
    }
}
