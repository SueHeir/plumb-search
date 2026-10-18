//! `plumb-tools embed-pages` and `page-meaning`: vectors of a page set's
//! pages, made from each page's title, description and snippet (an
//! article's lead), and a check of which pages searches find by meaning.
//!
//! Nodes find pages by their words only, so "polio iron lung" never finds
//! the article Iron lung. These measure how many such pages meaning would
//! find before any node searches pages that way.

use plumb_core::locks::{Locked, RwLocked};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Args;
use plumb_embed::{page_text, text_hash, Embedder, Vectors, MODEL_BASE_URL};
use plumb_node::block_on;
use plumb_node::cli::parse_positive;
use plumb_node::meaning::{
    embed_sites, ensure_model, load_embedder, load_vectors_for, page_vector_files, ToEmbed,
    QUERY_INSTRUCTION,
};

/// Pages embedded between saves of the vectors file. A save writes the
/// whole file, so saving every 10,000 as sites do would write about 100 GB
/// over two million pages.
const PAGES_PER_SAVE: usize = 100_000;

/// The longest key a vectors file holds, in bytes.
const MAX_KEY: usize = 255;

#[derive(Debug, Args)]
pub struct EmbedPagesArgs {
    /// Directory of a page set's items, as a node holds them
    /// (`DATA/sets/plumb/wikipedia-en`).
    #[arg(long, value_name = "DIR")]
    pub set: PathBuf,
    /// Directory of the model's files, downloaded when missing.
    #[arg(long, value_name = "DIR")]
    pub model: PathBuf,
    /// Vectors file, keyed by page URL, created or brought up to date: a
    /// page whose text is unchanged keeps its vector, so a stopped run goes
    /// on where it stopped.
    #[arg(long, value_name = "FILE")]
    pub vectors: PathBuf,
    /// Embed only the first N pages, in the set's order (most read first)
    /// [default: all].
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub max: Option<usize>,
    /// Texts embedded at once [default: one per CPU].
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub threads: Option<usize>,
    /// Embed pages with no snippet too, from their title and description
    /// alone. A set can't carry their vectors yet, as the data format pins
    /// a vector to its item's text.
    #[arg(long)]
    pub without_snippet: bool,
}

/// `plumb-tools embed-pages`: a vector for every page of the set that has
/// a snippet, made from [`page_text`], a batch at a time so only one
/// batch's texts are held. Pages without a snippet have no vector unless
/// `--without-snippet` is given, as a vector in the data format is pinned
/// to its item's text ([`plumb_format::Item::is_current`]).
pub fn run_embed_pages(args: EmbedPagesArgs) -> Result<()> {
    block_on(ensure_model(&args.model, MODEL_BASE_URL))??;
    let embedder = load_embedder(&args.model)?;
    // Saving writes beside the file, which a bare file name has no folder for.
    let path = std::path::absolute(&args.vectors)?;
    let vectors = RwLock::new(load_vectors_for(&path, &embedder)?);
    let threads = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let max = args.max.unwrap_or(usize::MAX);
    let without_snippet = args.without_snippet;
    let started = Instant::now();
    let (mut seen, mut kept, mut long, mut embedded, mut failed) =
        (0usize, HashSet::new(), 0, 0, 0);
    let mut batch: Vec<ToEmbed> = Vec::new();
    let mut flush = |batch: &mut Vec<ToEmbed>| -> Result<()> {
        let done = embed_sites(
            &embedder,
            &vectors,
            std::mem::take(batch),
            threads,
            &|| false,
            &mut |_| Ok(()),
            &mut |_, _| {},
        )?;
        embedded += done.done;
        failed += done.failed;
        Vectors::save_shared(&vectors, &path)?;
        println!(
            "{embedded} pages embedded ({:.0} a second), {} vectors",
            embedded as f64 / started.elapsed().as_secs_f64().max(0.001),
            vectors.read_locked().len()
        );
        Ok(())
    };
    plumb_node::sets::for_each_item(&args.set, |item| {
        let skipped = item.text.is_none() && !without_snippet;
        if seen >= max || item.is_extension() || item.is_deleted() || skipped {
            return Ok(());
        }
        let Some(url) = item.url else {
            return Ok(());
        };
        let text = page_text(
            item.title.as_deref(),
            item.about.as_deref(),
            item.text.as_deref(),
            embedder.text_words(),
        );
        if text.is_empty() {
            return Ok(());
        }
        seen += 1;
        if url.len() > MAX_KEY {
            long += 1;
            return Ok(());
        }
        let hash = text_hash(&text);
        kept.insert(url.clone());
        if vectors.read_locked().get(&url).map(|(saved, _)| saved) != Some(&hash) {
            batch.push((item.weight.unwrap_or(0.0) as f32, url, hash, text));
            if batch.len() >= PAGES_PER_SAVE {
                flush(&mut batch)?;
            }
        }
        Ok(())
    })?;
    flush(&mut batch)?;
    // Pages gone from the set, or past --max now, lose their vectors.
    vectors.write_locked().retain(|url| kept.contains(url));
    Vectors::save_shared(&vectors, &path)?;
    println!(
        "{} page vectors in {} ({seen} pages with text, {embedded} embedded now, {failed} failed, {long} with a URL too long to key)",
        vectors.read_locked().len(),
        path.display(),
    );
    Ok(())
}

#[derive(Debug, Args)]
pub struct PageMeaningArgs {
    /// Directory of the model that made the vectors.
    #[arg(long, value_name = "DIR")]
    pub model: PathBuf,
    /// Page vectors `embed-pages` made, a file or a directory of them. Given
    /// more than once, or as a directory, each search is reported for each
    /// file.
    #[arg(long, value_name = "PATH", required = true)]
    pub vectors: Vec<PathBuf>,
    /// Searches, one per line: `id<TAB>query<TAB>title`, where the title is
    /// the English Wikipedia article the search wants, or `-` for none.
    #[arg(long, value_name = "FILE")]
    pub queries: PathBuf,
    /// Where each search's result goes, as TSV.
    #[arg(long, value_name = "FILE")]
    pub out: PathBuf,
    /// Nearest pages looked at for each search.
    #[arg(long, value_name = "N", default_value_t = 100, value_parser = parse_positive)]
    pub nearest: usize,
    /// Searches run at once [default: one per CPU].
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub threads: Option<usize>,
}

/// What one search found by meaning, as the query is (`plain`) and after
/// the model's query instruction (`instructed`).
struct Found {
    id: String,
    query: String,
    wanted: String,
    /// The vectors file's name without `.bin`.
    set: String,
    held: bool,
    plain: Nearest,
    instructed: Nearest,
}

/// Where the wanted page came among the nearest (1 is nearest, 0 not
/// among them), the cosines of the nearest and of the tenth, and the
/// titles of the first three.
#[derive(Default)]
struct Nearest {
    rank: usize,
    first: f32,
    tenth: f32,
    titles: Vec<String>,
}

/// `plumb-tools page-meaning`: for each search and vectors file, the
/// pages nearest it in meaning and where the wanted article comes among
/// them, written to `--out`, with recall at 1, 3, 10 and `--nearest`
/// printed for searches whose article has a vector.
pub fn run_page_meaning(args: PageMeaningArgs) -> Result<()> {
    let embedder = load_embedder(&args.model)?;
    let mut sets = Vec::new();
    for path in &args.vectors {
        for file in page_vector_files(path)? {
            let name = file
                .file_stem()
                .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
            sets.push((name, Vectors::load(&file)?));
        }
    }
    let mut searches = Vec::new();
    for line in BufReader::new(
        File::open(&args.queries).with_context(|| format!("opening {}", args.queries.display()))?,
    )
    .lines()
    {
        let line = line?;
        let mut parts = line.split('\t');
        let (Some(id), Some(query)) = (parts.next(), parts.next()) else {
            continue;
        };
        let wanted = parts.next().unwrap_or("-").trim();
        searches.push((id.to_string(), query.to_string(), wanted.to_string()));
    }
    let threads = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let next = AtomicUsize::new(0);
    let found = Mutex::new(Vec::with_capacity(searches.len()));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                while let Some((id, query, wanted)) =
                    searches.get(next.fetch_add(1, Ordering::Relaxed))
                {
                    let key = title_key(wanted);
                    let url = plumb_core::article::article_url("en", wanted);
                    let plain = embedder.embed_query(query).ok();
                    let instructed = instructed_vector(&embedder, query);
                    let rows: Vec<Found> = sets
                        .iter()
                        .map(|(set, vectors)| {
                            let near = |vector: &Option<Vec<i8>>| {
                                vector.as_deref().map_or_else(Nearest::default, |vector| {
                                    nearest(vectors, vector, args.nearest, &key)
                                })
                            };
                            Found {
                                id: id.clone(),
                                query: query.clone(),
                                wanted: wanted.clone(),
                                set: set.clone(),
                                held: wanted != "-" && vectors.get(&url).is_some(),
                                plain: near(&plain),
                                instructed: near(&instructed),
                            }
                        })
                        .collect();
                    found.locked().extend(rows);
                }
            });
        }
    });
    let mut found = found.into_inner().unwrap_or_else(|err| err.into_inner());
    found.sort_by(|a, b| (&a.id, &a.set).cmp(&(&b.id, &b.set)));
    let mut out = BufWriter::new(
        File::create(&args.out).with_context(|| format!("creating {}", args.out.display()))?,
    );
    writeln!(
        out,
        "id\tquery\twanted\theld\tplain_rank\tplain_first\tplain_tenth\tplain_titles\tinstructed_rank\tinstructed_first\tinstructed_tenth\tinstructed_titles\tset"
    )?;
    for f in &found {
        writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{:.3}\t{:.3}\t{}\t{}\t{:.3}\t{:.3}\t{}\t{}",
            f.id,
            f.query,
            f.wanted,
            u8::from(f.held),
            f.plain.rank,
            f.plain.first,
            f.plain.tenth,
            f.plain.titles.join(" | "),
            f.instructed.rank,
            f.instructed.first,
            f.instructed.tenth,
            f.instructed.titles.join(" | "),
            f.set,
        )?;
    }
    out.flush()?;
    let held: Vec<&Found> = found.iter().filter(|f| f.held).collect();
    println!(
        "{} searches in {} vectors files, {} want an article, {} vectors of those articles",
        searches.len(),
        sets.len(),
        searches
            .iter()
            .filter(|(_, _, wanted)| wanted != "-")
            .count(),
        held.len()
    );
    for (name, of) in [
        ("plain", (|f: &Found| f.plain.rank) as fn(&Found) -> usize),
        ("instructed", |f: &Found| f.instructed.rank),
    ] {
        let within = |n: usize| held.iter().filter(|f| (1..=n).contains(&of(f))).count();
        let share = |n: usize| 100.0 * within(n) as f64 / held.len().max(1) as f64;
        println!(
            "{name}: wanted article first {:.1}%, in 3 {:.1}%, in 10 {:.1}%, in {} {:.1}%",
            share(1),
            share(3),
            share(10),
            args.nearest,
            share(args.nearest)
        );
    }
    println!("per search: {}", args.out.display());
    Ok(())
}

/// The vector of `query` after the model's query instruction, as a node
/// makes it with `--query-instruction on`; `None` for a model that takes
/// none.
fn instructed_vector(embedder: &Embedder, query: &str) -> Option<Vec<i8>> {
    embedder
        .takes_query_instruction()
        .then(|| embedder.embed(&format!("{QUERY_INSTRUCTION}{query}")).ok())
        .flatten()
}

/// The `k` pages nearest `vector`, and where the page whose
/// [`title_key`] is `wanted` came among them.
fn nearest(vectors: &Vectors, vector: &[i8], k: usize, wanted: &str) -> Nearest {
    let found = vectors.nearest(vector, k);
    Nearest {
        rank: found
            .iter()
            .position(|(url, _)| title_key(url_title(url)) == wanted)
            .map_or(0, |at| at + 1),
        first: found.first().map_or(0.0, |&(_, cosine)| cosine),
        tenth: found.get(9).map_or(0.0, |&(_, cosine)| cosine),
        titles: found
            .iter()
            .take(3)
            .map(|&(url, cosine)| format!("{} {cosine:.3}", title_key(url_title(url))))
            .collect(),
    }
}

/// The title part of an article's URL, as written there.
fn url_title(url: &str) -> &str {
    url.rsplit_once("/wiki/").map_or(url, |(_, title)| title)
}

/// A title, or the title part of a URL, in one form for comparing:
/// unescaped, underscores as spaces, in lower case.
fn title_key(title: &str) -> String {
    percent_encoding::percent_decode_str(title)
        .decode_utf8_lossy()
        .replace('_', " ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use plumb_format::Item;

    use super::*;

    #[test]
    fn title_keys_match_across_url_forms() {
        assert_eq!(
            title_key(url_title("https://en.wikipedia.org/wiki/Iron_lung")),
            "iron lung"
        );
        assert_eq!(
            title_key(url_title("https://en.wikipedia.org/wiki/Caf%C3%A9_au_lait")),
            title_key("Café au lait")
        );
        assert_eq!(title_key("Marie Curie"), "marie curie");
    }

    #[test]
    fn embeds_pages_with_a_snippet_and_finds_them_by_meaning() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("model");
        plumb_embed::write_test_model(&model).unwrap();
        let article = |title: &str, about: &str, text: Option<&str>| {
            let mut item: Item = serde_json::from_value(serde_json::json!({
                "id": title, "kind": "article", "title": title, "about": about,
                "url": plumb_core::article::article_url("en", title),
            }))
            .unwrap();
            item.text = text.map(str::to_string);
            Some(item)
        };
        let set = dir.path().join("set");
        let manifest = serde_json::json!({
            "format": plumb_format::FORMAT, "set": "plumb/wikipedia-en",
            "generation": "2026-10-12T04:00:00Z", "license": "CC-BY-SA-4.0",
        });
        crate::set::write_set(
            [
                article("Tesla", "electric car maker", Some("tesla electric cars")),
                article("Credit union", "bank", Some("a credit union bank")),
                article("Paper", "news paper", None),
            ]
            .into_iter(),
            Default::default(),
            manifest.as_object().unwrap().clone(),
            &set,
            &[],
            3,
        )
        .unwrap();
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        crate::set::sign_set(&set, &key, None, None).unwrap();
        let vectors = dir.path().join("pages.bin");
        let args = |max| EmbedPagesArgs {
            set: set.clone(),
            model: model.clone(),
            vectors: vectors.clone(),
            max,
            threads: Some(2),
            without_snippet: false,
        };
        run_embed_pages(args(None)).unwrap();
        let saved = Vectors::load(&vectors).unwrap();
        assert_eq!(saved.len(), 2, "a page with no snippet gets no vector");
        let tesla = plumb_core::article::article_url("en", "Tesla");
        assert!(saved.get(&tesla).is_some());

        // Fewer pages asked for: the first one stays, the rest go.
        run_embed_pages(args(Some(1))).unwrap();
        let saved = Vectors::load(&vectors).unwrap();
        assert_eq!(saved.len(), 1);
        assert!(saved.get(&tesla).is_some());

        // Asked for, a page with no snippet gets one from its title.
        run_embed_pages(EmbedPagesArgs {
            without_snippet: true,
            ..args(None)
        })
        .unwrap();
        let paper = plumb_core::article::article_url("en", "Paper");
        assert!(Vectors::load(&vectors).unwrap().get(&paper).is_some());
        run_embed_pages(args(None)).unwrap();

        let queries = dir.path().join("queries.tsv");
        std::fs::write(&queries, "q1\telectric cars\tTesla\nq2\tsolar\t-\n").unwrap();
        let out = dir.path().join("out.tsv");
        // A directory of vectors files: each search is reported for each.
        let more = dir.path().join("more");
        std::fs::create_dir(&more).unwrap();
        std::fs::copy(&vectors, more.join("copy.bin")).unwrap();
        run_page_meaning(PageMeaningArgs {
            model,
            vectors: vec![vectors, more],
            queries,
            out: out.clone(),
            nearest: 2,
            threads: Some(1),
        })
        .unwrap();
        // The test model's vectors mean nothing, so only check that the
        // wanted article is held and among the two nearest in both.
        let written = std::fs::read_to_string(out).unwrap();
        assert_eq!(written.lines().count(), 5);
        for (line, set) in written.lines().skip(1).zip(["copy", "pages"]) {
            let row: Vec<&str> = line.split('\t').collect();
            assert_eq!(row[..4], ["q1", "electric cars", "Tesla", "1"]);
            assert!(["1", "2"].contains(&row[4]), "{row:?}");
            assert_eq!(row[12], set);
        }
    }
}
