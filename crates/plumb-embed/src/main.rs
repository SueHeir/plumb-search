//! `plumb-embed`: the homepage embedding prototype.
//!
//! - `plumb-embed page --url URL FILE.html` prints the page's embedding
//!   text, its hash, the model's hashes and the vector's hash as JSON.
//! - `plumb-embed text TEXT` does the same for a text.
//! - `--check` embeds again with one thread and fails unless the bytes match.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use plumb_embed::{page_text, text_hash, vector_hash, Embedder, MODEL_NAME};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "plumb-embed",
    about = "Deterministic homepage embeddings (prototype)"
)]
struct Cli {
    /// Where the model is kept (downloaded on first use)
    #[arg(long, global = true, default_value = ".fastembed_cache")]
    cache: PathBuf,
    /// ONNX Runtime threads
    #[arg(long, global = true, default_value_t = 8)]
    threads: usize,
    /// Embed a second time with one thread and fail unless the bytes match
    #[arg(long, global = true)]
    check: bool,
    /// Print the whole vector, not just its hash
    #[arg(long, global = true)]
    full: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Embed a saved homepage
    Page {
        /// The address the page was fetched from
        #[arg(long)]
        url: url::Url,
        /// The page's HTML
        file: PathBuf,
    },
    /// Embed a text as given
    Text { text: Vec<String> },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let text = match &cli.command {
        Command::Page { url, file } => {
            let html =
                std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
            page_text(&plumb_crawl::extract_page_meta(
                url,
                &String::from_utf8_lossy(&html),
            ))
        }
        Command::Text { text } => text.join(" "),
    };
    if text.is_empty() {
        bail!("the page has no text to embed");
    }
    let mut embedder = Embedder::new(&cli.cache, cli.threads)?;
    let vector = embedder.embed(std::slice::from_ref(&text))?.remove(0);
    if cli.check {
        let again = Embedder::new(&cli.cache, 1)?
            .embed(std::slice::from_ref(&text))?
            .remove(0);
        if again != vector {
            bail!(
                "one thread gave different bytes than {} threads",
                cli.threads
            );
        }
    }
    let mut out = json!({
        "text": text,
        "text_sha256": text_hash(&text),
        "model": MODEL_NAME,
        "model_files_sha256": Embedder::model_hashes(&cli.cache)?
            .into_iter()
            .map(|(file, hash)| (file, json!(hash)))
            .collect::<serde_json::Map<_, _>>(),
        "dimensions": vector.len(),
        "vector_sha256": vector_hash(&vector),
        "vector_head": &vector[..8],
    });
    if cli.full {
        out["vector"] = json!(vector);
    }
    if cli.check {
        out["check"] = json!("1 thread and the configured threads gave the same bytes");
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
