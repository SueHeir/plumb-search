//! `plumb fetch-text` and `plumb terms`: an experiment in picking a site's
//! search terms from its whole homepage, rather than searching only its
//! title, description, headings and link text.
//!
//! `fetch-text` fetches homepages once and keeps their visible text (up to
//! [`plumb_crawl::MAX_PAGE_TEXT_WORDS`] words) in a pages file. `terms` then picks each
//! page's terms with one of several [`Method`]s and writes them into the
//! records' `terms` field, so `plumb index` and `plumb eval --rank
//! '{"terms_boost": ...}'` can compare the methods on the same pages.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use plumb_core::{SiteRecord, MAX_TERMS};
use plumb_crawl::{CrawlConfig, CrawlOutcome, HomepageCrawler};
use plumb_embed::SparseModel;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::cli::parse_positive;
use crate::crawl::target_for;
use crate::records::{load_records, replace_records, sorted_by_link_score};
use crate::runtime;

#[derive(Debug, Args)]
pub struct FetchTextArgs {
    /// Records file to pick homepages from (JSON lines).
    #[arg(long, value_name = "PATH")]
    pub records: PathBuf,
    /// How many homepages to fetch, best link score first.
    #[arg(long, value_name = "N", default_value_t = 20_000)]
    pub top: usize,
    /// Also fetch these sites: one domain per line, or eval queries files
    /// (`query<TAB>domain[,domain]`), whose expected domains are taken.
    /// Can be given more than once.
    #[arg(long, value_name = "FILE")]
    pub domains: Vec<PathBuf>,
    /// Pages file to write (JSON lines).
    #[arg(long, value_name = "PATH")]
    pub out: PathBuf,
    /// Homepage fetches in flight at once.
    #[arg(long, value_name = "N", default_value_t = 64, value_parser = parse_positive)]
    pub concurrency: usize,
    /// Host name lookups in flight at once.
    #[arg(long, value_name = "N", default_value_t = 32, value_parser = parse_positive)]
    pub dns_lookups: usize,
    /// Fetch through the proxy in HTTP_PROXY, HTTPS_PROXY or ALL_PROXY.
    #[arg(long)]
    pub use_system_proxy: bool,
}

/// How `plumb terms` picks a page's terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Method {
    /// The page's first distinct words, in order: what searching the
    /// page's own text would give.
    First,
    /// YAKE, a statistical keyword extractor: no model.
    Yake,
    /// A learned sparse model (--model), which can add words the page
    /// does not use.
    Sparse,
}

#[derive(Debug, Args)]
pub struct TermsArgs {
    /// Pages file made by `plumb fetch-text`.
    #[arg(long, value_name = "PATH")]
    pub pages: PathBuf,
    /// Records file whose sites get the terms (JSON lines).
    #[arg(long, value_name = "PATH")]
    pub records: PathBuf,
    /// Where to write the records with terms. Sites with no page keep no
    /// terms.
    #[arg(long, value_name = "PATH")]
    pub out: PathBuf,
    #[arg(long, value_enum)]
    pub method: Method,
    /// Directory of the sparse model: config.json, tokenizer.json and
    /// model.safetensors (or pytorch_model.bin).
    #[arg(long, value_name = "DIR", required_if_eq("method", "sparse"))]
    pub model: Option<PathBuf>,
    /// Distinct words picked per page.
    #[arg(long, value_name = "N", default_value_t = 100, value_parser = parse_positive)]
    pub count: usize,
    /// Most tokens of a page the sparse model reads (at most 512).
    #[arg(long, value_name = "N", default_value_t = 512, value_parser = parse_positive)]
    pub max_tokens: usize,
    /// Pages worked on at once [default: one per CPU].
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub threads: Option<usize>,
}

/// One homepage's text, a line of the pages file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PageText {
    pub domain: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headings: Vec<String>,
    #[serde(default)]
    pub text: String,
}

impl PageText {
    /// Everything the page says, title first.
    fn full_text(&self) -> String {
        self.title
            .iter()
            .chain(&self.description)
            .chain(&self.headings)
            .chain(std::iter::once(&self.text))
            .map(|part| part.trim())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(". ")
    }
}

/// `plumb fetch-text`.
pub fn run_fetch_text(args: FetchTextArgs) -> Result<()> {
    let records = load_records(&args.records)?;
    let mut wanted: Vec<&SiteRecord> = sorted_by_link_score(&records)
        .into_iter()
        .filter(|r| r.redirect.is_none())
        .take(args.top)
        .collect();
    let mut seen: HashSet<&str> = wanted.iter().map(|r| r.domain.as_str()).collect();
    let mut missing = 0;
    for path in &args.domains {
        for domain in read_domains(path)? {
            match records.get(&domain) {
                Some(record) if seen.insert(record.domain.as_str()) => wanted.push(record),
                Some(_) => {}
                None => missing += 1,
            }
        }
    }
    if missing > 0 {
        info!("{missing} listed domains have no record and are skipped");
    }
    let targets: Vec<_> = wanted.iter().map(|r| target_for(r)).collect();
    let total = targets.len();
    println!("fetching {total} homepages, {} at a time", args.concurrency);

    let cfg = CrawlConfig {
        concurrency: args.concurrency,
        dns_lookups: args.dns_lookups,
        use_system_proxy: args.use_system_proxy,
        ..CrawlConfig::default()
    };
    let mut out = BufWriter::new(
        File::create(&args.out).with_context(|| format!("creating {}", args.out.display()))?,
    );
    let (mut done, mut fetched) = (0usize, 0usize);
    runtime()?.block_on(async {
        let mut crawler = HomepageCrawler::new(cfg);
        crawler.push(targets);
        while let Some(result) = crawler.next().await {
            done += 1;
            if let CrawlOutcome::Fetched(page) = result.outcome {
                let meta = page.meta;
                let line = PageText {
                    domain: page.domain,
                    title: meta.title,
                    description: meta.description,
                    headings: meta.headings,
                    text: meta.page_text,
                };
                serde_json::to_writer(&mut out, &line)?;
                out.write_all(b"\n")?;
                fetched += 1;
            }
            if done % 1000 == 0 {
                info!("{done} of {total} homepages tried, {fetched} fetched");
            }
        }
        anyhow::Ok(())
    })?;
    out.flush()?;
    println!(
        "fetched {fetched} of {total} homepages into {}",
        args.out.display()
    );
    Ok(())
}

/// The domains in `path`: one per line, or the expected domains of an eval
/// queries file.
fn read_domains(path: &Path) -> Result<Vec<String>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut domains = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let field = line.split('\t').nth(1).unwrap_or(line);
        domains.extend(
            field
                .split(',')
                .map(|d| d.trim().to_lowercase())
                .filter(|d| !d.is_empty()),
        );
    }
    Ok(domains)
}

/// A page's index in the pages file, its terms and the milliseconds they took.
type Picked = (usize, Result<Vec<String>>, f64);

/// `plumb terms`.
pub fn run_terms(args: TermsArgs) -> Result<()> {
    let pages: Vec<PageText> = plumb_core::read_jsonl(&args.pages)?;
    let mut records = load_records(&args.records)?;
    let picker = Picker::new(&args)?;
    let threads = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
        .min(pages.len().max(1));

    let started = Instant::now();
    let next = Mutex::new(0usize);
    let results: Mutex<Vec<Picked>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let i = {
                    let mut next = next.lock().unwrap();
                    let i = *next;
                    *next += 1;
                    i
                };
                let Some(page) = pages.get(i) else { break };
                let one = Instant::now();
                let terms = picker.terms(&page.full_text());
                let ms = one.elapsed().as_secs_f64() * 1000.0;
                results.lock().unwrap().push((i, terms, ms));
                if i > 0 && i % 1000 == 0 {
                    info!("{i} of {} pages", pages.len());
                }
            });
        }
    });
    let wall = started.elapsed().as_secs_f64();

    let mut times = Vec::new();
    let (mut with_terms, mut failed, mut words, mut bytes) = (0usize, 0usize, 0usize, 0usize);
    for (i, terms, ms) in results.into_inner().unwrap() {
        times.push(ms);
        let terms = match terms {
            Ok(terms) => terms,
            Err(err) => {
                failed += 1;
                info!("{}: {err:#}", pages[i].domain);
                continue;
            }
        };
        if terms.is_empty() || records.get(&pages[i].domain).is_none() {
            continue;
        }
        with_terms += 1;
        words += terms.iter().collect::<HashSet<_>>().len();
        bytes += terms.iter().map(|t| t.len() + 1).sum::<usize>();
        records.entry(&pages[i].domain).terms = terms;
    }
    let written = replace_records(&args.out, sorted_by_link_score(&records))?;

    times.sort_by(f64::total_cmp);
    let pct = |p: f64| {
        times
            .get(((times.len() as f64 - 1.0) * p) as usize)
            .copied()
    };
    println!(
        "{:?}: {} pages in {wall:.1}s on {threads} threads; per page {:.1} ms median, \
         {:.1} ms at 95%",
        args.method,
        pages.len(),
        pct(0.5).unwrap_or(0.0),
        pct(0.95).unwrap_or(0.0),
    );
    if with_terms > 0 {
        println!(
            "{with_terms} sites got terms ({failed} failed): {:.0} distinct words and {:.0} \
             bytes each on average",
            words as f64 / with_terms as f64,
            bytes as f64 / with_terms as f64
        );
    }
    println!("wrote {written} records to {}", args.out.display());
    Ok(())
}

/// Picks terms by one [`Method`].
enum Picker {
    First(usize),
    Yake(usize, yake_rust::StopWords),
    Sparse(usize, Box<SparseModel>),
}

impl Picker {
    fn new(args: &TermsArgs) -> Result<Self> {
        let count = args.count.min(MAX_TERMS);
        Ok(match args.method {
            Method::First => Picker::First(count),
            Method::Yake => Picker::Yake(
                count,
                yake_rust::StopWords::predefined("en").context("YAKE's English stop words")?,
            ),
            Method::Sparse => {
                let Some(dir) = &args.model else {
                    bail!("--method sparse needs --model");
                };
                Picker::Sparse(
                    count,
                    Box::new(SparseModel::load(dir, args.max_tokens.min(512))?),
                )
            }
        })
    }

    /// Up to `count` distinct words of `text`, best first, the best
    /// repeated so they weigh more (see [`repeated`]); at most
    /// [`MAX_TERMS`] entries in all.
    fn terms(&self, text: &str) -> Result<Vec<String>> {
        let words = match self {
            Picker::First(count) => {
                let mut seen = HashSet::new();
                return Ok(words_of(text)
                    .filter(|w| seen.insert(w.clone()))
                    .take(*count)
                    .collect());
            }
            Picker::Yake(count, stop_words) => {
                let config = yake_rust::Config {
                    ngrams: 2,
                    ..yake_rust::Config::default()
                };
                let mut seen = HashSet::new();
                let words: Vec<String> =
                    yake_rust::get_n_best(*count * 2, text, stop_words, &config)
                        .into_iter()
                        .flat_map(|item| words_of(&item.keyword).collect::<Vec<_>>())
                        .filter(|w| seen.insert(w.clone()))
                        .take(*count)
                        .collect();
                // YAKE's scores do not compare between pages; its order does.
                let n = words.len().max(1) as f32;
                words
                    .into_iter()
                    .enumerate()
                    .map(|(i, w)| (w, 1.0 - i as f32 / n))
                    .collect::<Vec<_>>()
            }
            Picker::Sparse(count, model) => model
                .words(text, *count)?
                .into_iter()
                .map(|w| (w.word, w.weight))
                .collect(),
        };
        Ok(repeated(&words))
    }
}

/// Lowercased words of letters and digits, two characters or more.
fn words_of(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() > 1)
        .map(str::to_lowercase)
}

/// `words` (best first, with weights) as terms: a word weighing at least
/// two thirds of the heaviest stands three times, at least a third twice,
/// others once; at most [`MAX_TERMS`] entries.
fn repeated(words: &[(String, f32)]) -> Vec<String> {
    let top = words.iter().map(|(_, w)| *w).fold(0.0f32, f32::max);
    let mut terms = Vec::new();
    for (word, weight) in words {
        let share = if top > 0.0 { weight / top } else { 0.0 };
        let times = 1 + usize::from(share >= 1.0 / 3.0) + usize::from(share >= 2.0 / 3.0);
        for _ in 0..times {
            if terms.len() == MAX_TERMS {
                return terms;
            }
            terms.push(word.clone());
        }
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heavier_words_stand_more_times() {
        let words = [
            ("tesla".to_string(), 3.0),
            ("car".to_string(), 1.5),
            ("the".to_string(), 0.3),
        ];
        assert_eq!(
            repeated(&words),
            ["tesla", "tesla", "tesla", "car", "car", "the"]
        );
    }

    #[test]
    fn first_and_yake_pick_distinct_words() {
        let args = |method| TermsArgs {
            pages: PathBuf::new(),
            records: PathBuf::new(),
            out: PathBuf::new(),
            method,
            model: None,
            count: 4,
            max_tokens: 512,
            threads: None,
        };
        let text = "Electric cars. Tesla builds electric cars, solar roofs and batteries. \
                    Order a Tesla electric car online today.";
        let first = Picker::new(&args(Method::First)).unwrap();
        assert_eq!(
            first.terms(text).unwrap(),
            ["electric", "cars", "tesla", "builds"]
        );
        let yake = Picker::new(&args(Method::Yake))
            .unwrap()
            .terms(text)
            .unwrap();
        let distinct: HashSet<_> = yake.iter().collect();
        assert!(!yake.is_empty() && distinct.len() <= 4);
        assert!(
            yake.iter().any(|w| w == "tesla" || w == "electric"),
            "{yake:?}"
        );
    }

    #[test]
    fn reads_domains_and_eval_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.tsv");
        std::fs::write(
            &path,
            "# c\nelectric car maker\ttesla.com,Rivian.com\nexample.org\n",
        )
        .unwrap();
        assert_eq!(
            read_domains(&path).unwrap(),
            ["tesla.com", "rivian.com", "example.org"]
        );
    }
}
