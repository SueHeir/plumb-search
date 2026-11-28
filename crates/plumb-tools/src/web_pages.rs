//! `plumb-tools web-pages`: the web index (plumb-index's `web`), built
//! from the pages an open crawl holds of popular sites, embedded, and
//! searched from the command line.
//!
//! `build` reads pages kept from Common Crawl's text files (id, address,
//! title and first words, a line a page, in `part-NNNNN.tsv.gz` files)
//! into the words index, and writes each page's title and lead in row
//! order for `embed`. `embed` makes their vectors and signs (plumb-embed's
//! `RowVectors`) and goes on where it stopped. `search` lists a query's pages as a node would put
//! them in order: the first by words and by meaning, fused.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use plumb_embed::{
    page_text, rows_header, signs, Embedder, RowVectors, MODEL_BASE_URL, ROWS_HEADER_LEN,
};
use plumb_index::web::{
    create_web_words, fuse, WebWords, WEB_FIRST, WEB_READ_AGAIN, WEB_SIGNS_FILE, WEB_VECTORS_FILE,
    WEB_WORDS_DIR,
};
use plumb_node::block_on;
use plumb_node::cli::parse_positive;
use plumb_node::meaning::{ensure_model, load_embedder, QUERY_INSTRUCTION};
use tantivy::TantivyDocument;

/// Each page's title and lead in row order, which `build` writes for `embed`.
const TEXTS_FILE: &str = "texts.tsv.zst";
/// Pages embedded between writes of the vectors, so a stopped run keeps
/// most of its work.
const EMBED_EVERY: usize = 10_000;

#[derive(Debug, Args)]
pub struct WebPagesArgs {
    #[command(subcommand)]
    pub command: WebPagesCommand,
}

#[derive(Debug, Subcommand)]
pub enum WebPagesCommand {
    /// Build a web index's words from crawl pages.
    Build {
        /// Folder of `part-NNNNN.tsv.gz` files of pages (`wet_pages.py`'s).
        #[arg(long, value_name = "DIR")]
        pages: PathBuf,
        /// The web index's folder, made new.
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
        /// Words of a page's text shown under its title and embedded
        /// with it: in the page-index trial 50 listed nearly as many
        /// searches' pages as 200 (410 of 1,000 against 420), and embed
        /// half again as fast.
        #[arg(long, value_name = "N", default_value_t = 50, value_parser = parse_positive)]
        lead_words: usize,
        /// Memory for indexing, in MB.
        #[arg(long, value_name = "MB", default_value_t = 2_000, value_parser = parse_positive)]
        memory_mb: usize,
    },
    /// Make the vectors of a web index's pages, going on where a stopped
    /// run left off.
    Embed {
        /// The web index's folder.
        #[arg(long, value_name = "DIR")]
        dir: PathBuf,
        /// Directory of the model's files, downloaded when missing.
        #[arg(long, value_name = "DIR")]
        model: PathBuf,
        /// Pages embedded at once [default: one per CPU].
        #[arg(long, value_name = "N", value_parser = parse_positive)]
        threads: Option<usize>,
    },
    /// List a query's pages in a web index, by words and meaning fused.
    Search {
        /// The web index's folder.
        #[arg(long, value_name = "DIR")]
        dir: PathBuf,
        /// The model that made its vectors; without it, by words only.
        #[arg(long, value_name = "DIR")]
        model: Option<PathBuf>,
        /// Pages to print.
        #[arg(long, value_name = "N", default_value_t = 10, value_parser = parse_positive)]
        show: usize,
        query: String,
    },
}

pub fn run(args: WebPagesArgs) -> Result<()> {
    match args.command {
        WebPagesCommand::Build {
            pages,
            out,
            lead_words,
            memory_mb,
        } => build(&pages, &out, lead_words, memory_mb),
        WebPagesCommand::Embed {
            dir,
            model,
            threads,
        } => {
            let threads = threads
                .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
            embed(&dir, &model, threads)
        }
        WebPagesCommand::Search {
            dir,
            model,
            show,
            query,
        } => search(&dir, model.as_deref(), show, &query),
    }
}

/// The `part-*.tsv.gz` files of `dir`, in name order.
fn page_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("listing {}", dir.display()))?
        .map(|entry| Ok(entry?.path()))
        .collect::<std::io::Result<_>>()?;
    files.retain(|file| {
        file.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("part-") && name.ends_with(".tsv.gz"))
    });
    files.sort();
    Ok(files)
}

/// `text` with no tab or line break, which the texts file separates by.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn build(pages: &Path, out: &Path, lead_words: usize, memory_mb: usize) -> Result<()> {
    if out.exists() && std::fs::read_dir(out)?.next().is_some() {
        bail!(
            "{} is not empty: a web index is built into a new folder",
            out.display()
        );
    }
    let words = out.join(WEB_WORDS_DIR);
    std::fs::create_dir_all(&words)?;
    let (index, fields) = create_web_words(&words)?;
    let mut writer = index.writer::<TantivyDocument>(memory_mb.saturating_mul(1_000_000))?;
    let mut texts = zstd::Encoder::new(BufWriter::new(File::create(out.join(TEXTS_FILE))?), 3)?;
    let files = page_files(pages)?;
    if files.is_empty() {
        bail!("no part-*.tsv.gz files in {}", pages.display());
    }
    let started = Instant::now();
    let (mut rows, mut skipped) = (0u32, 0usize);
    for (n, file) in files.iter().enumerate() {
        let lines = BufReader::new(flate2::read::MultiGzDecoder::new(File::open(file)?));
        for line in lines.lines() {
            let line = line?;
            let mut parts = line.splitn(4, '\t').skip(1);
            let (Some(url), Some(title), Some(body)) = (parts.next(), parts.next(), parts.next())
            else {
                skipped += 1;
                continue;
            };
            if !(url.starts_with("http://") || url.starts_with("https://"))
                || title.trim().is_empty()
            {
                skipped += 1;
                continue;
            }
            let lead: Vec<&str> = body.split_whitespace().take(lead_words).collect();
            let lead = lead.join(" ");
            let mut document = TantivyDocument::new();
            document.add_u64(fields.row, u64::from(rows));
            document.add_text(fields.url, url);
            document.add_text(fields.title, title);
            document.add_text(fields.body, body);
            document.add_text(fields.lead, &lead);
            writer.add_document(document)?;
            writeln!(texts, "{}\t{}", one_line(title), one_line(&lead))?;
            rows = rows
                .checked_add(1)
                .context("more pages than rows can number")?;
        }
        if (n + 1) % 100 == 0 || n + 1 == files.len() {
            println!(
                "{} of {} files, {rows} pages ({skipped} skipped), {:.0} s",
                n + 1,
                files.len(),
                started.elapsed().as_secs_f64()
            );
        }
    }
    writer.commit()?;
    writer.wait_merging_threads()?;
    texts.finish()?.flush()?;
    println!("{rows} pages in {}", out.display());
    Ok(())
}

/// Rows already embedded in both files at `dir`, which are cut to that
/// many, or new files of `embedder`'s vectors.
fn embedded_rows(dir: &Path, embedder: &Embedder) -> Result<(File, File, usize)> {
    let (dim, width) = (embedder.dim(), embedder.dim().div_ceil(8));
    let paths = [dir.join(WEB_VECTORS_FILE), dir.join(WEB_SIGNS_FILE)];
    if paths.iter().all(|path| path.exists()) {
        let rows = |path: &Path, of_signs: bool| -> Result<usize> {
            let mut header = vec![0; ROWS_HEADER_LEN];
            std::io::Read::read_exact(&mut File::open(path)?, &mut header)?;
            if header != rows_header(&embedder.id(), dim, of_signs) {
                bail!(
                    "{} was made by another model; remove it to embed again",
                    path.display()
                );
            }
            Ok((std::fs::metadata(path)?.len() as usize - ROWS_HEADER_LEN)
                / if of_signs { width } else { dim })
        };
        let done = rows(&paths[0], false)?.min(rows(&paths[1], true)?);
        let open = |path: &Path, row: usize| -> Result<File> {
            let file = OpenOptions::new().append(true).open(path)?;
            file.set_len((ROWS_HEADER_LEN + done * row) as u64)?;
            Ok(file)
        };
        return Ok((open(&paths[0], dim)?, open(&paths[1], width)?, done));
    }
    let create = |path: &Path, of_signs: bool| -> Result<File> {
        let mut file = File::create(path)?;
        file.write_all(&rows_header(&embedder.id(), dim, of_signs))?;
        Ok(file)
    };
    Ok((create(&paths[0], false)?, create(&paths[1], true)?, 0))
}

fn embed(dir: &Path, model: &Path, threads: usize) -> Result<()> {
    block_on(ensure_model(model, MODEL_BASE_URL))??;
    let embedder = load_embedder(model)?;
    let (mut vectors, mut bits, done) = embedded_rows(dir, &embedder)?;
    let texts = BufReader::new(zstd::Decoder::new(File::open(dir.join(TEXTS_FILE))?)?);
    let mut lines = texts.lines().skip(done);
    let (started, mut rows, mut failed) = (Instant::now(), done, 0usize);
    loop {
        let batch: Vec<String> = lines
            .by_ref()
            .take(EMBED_EVERY)
            .collect::<std::io::Result<_>>()?;
        if batch.is_empty() {
            break;
        }
        let made: Vec<Option<Vec<i8>>> = embed_batch(&embedder, &batch, threads);
        for vector in made {
            let vector = vector.unwrap_or_else(|| {
                failed += 1;
                vec![0; embedder.dim()]
            });
            vectors.write_all(&vector.iter().map(|&v| v as u8).collect::<Vec<u8>>())?;
            bits.write_all(&signs(&vector))?;
        }
        vectors.flush()?;
        bits.flush()?;
        rows += batch.len();
        println!(
            "{rows} pages embedded ({:.0} a second, {failed} failed)",
            (rows - done) as f64 / started.elapsed().as_secs_f64().max(0.001)
        );
    }
    vectors.sync_all()?;
    bits.sync_all()?;
    println!(
        "{rows} vectors in {} ({failed} failed, as zeros)",
        dir.display()
    );
    Ok(())
}

/// The vectors of `batch`'s pages (`title<TAB>lead` lines), in order, on
/// `threads` threads; `None` for a text the model could not embed.
fn embed_batch(embedder: &Embedder, batch: &[String], threads: usize) -> Vec<Option<Vec<i8>>> {
    let next = AtomicUsize::new(0);
    let mut made: Vec<(usize, Option<Vec<i8>>)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads.max(1))
            .map(|_| {
                scope.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(line) = batch.get(i) else {
                            return mine;
                        };
                        let (title, lead) = line.split_once('\t').unwrap_or((line, ""));
                        let text = page_text(Some(title), None, Some(lead), embedder.text_words());
                        mine.push((i, embedder.embed(&text).ok()));
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap_or_default())
            .collect()
    });
    made.sort_by_key(|(i, _)| *i);
    made.into_iter().map(|(_, vector)| vector).collect()
}

fn search(dir: &Path, model: Option<&Path>, show: usize, query: &str) -> Result<()> {
    let words = WebWords::open(dir)?;
    let by_words = words.by_words(query, WEB_FIRST)?;
    let by_meaning: Vec<u32> = match model {
        Some(model) => {
            let embedder = load_embedder(model)?;
            let vectors = RowVectors::open(&dir.join(WEB_VECTORS_FILE), &dir.join(WEB_SIGNS_FILE))?;
            if vectors.model() != embedder.id() || vectors.len() as u64 != words.len() {
                bail!(
                    "the vectors in {} are not this model's, or not all made yet",
                    dir.display()
                );
            }
            let vector = embedder.embed(&format!("{QUERY_INSTRUCTION}{query}"))?;
            let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
            let found = vectors.nearest(&vector, WEB_FIRST, WEB_READ_AGAIN, threads);
            found.into_iter().map(|(row, _)| row).collect()
        }
        None => Vec::new(),
    };
    let rank = |list: &[u32], row: u32| {
        list.iter()
            .position(|&r| r == row)
            .map_or("-".to_string(), |at| (at + 1).to_string())
    };
    println!("rank\twords\tmeaning\turl\ttitle");
    for (at, row) in fuse(&by_words, &by_meaning)
        .into_iter()
        .take(show)
        .enumerate()
    {
        let Some(page) = words.page(row)? else {
            continue;
        };
        println!(
            "{}\t{}\t{}\t{}\t{}",
            at + 1,
            rank(&by_words, row),
            rank(&by_meaning, row),
            page.url,
            page.title
        );
    }
    Ok(())
}
