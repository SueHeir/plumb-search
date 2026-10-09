//! `plumb fact-trust`: how often each site's pages get Wikidata's facts
//! right (Knowledge-Based Trust, Dong et al. 2015), from the text of pages
//! in Common Crawl WET files, checked with [`plumb_core::fact_check`].
//!
//! Each site counts a fact it states once, however many of its pages
//! state it, so a site does not gain by repeating itself. With `--apply`,
//! a copy of a records file gets each site's counts
//! ([`Signals::fact_checks`], [`Signals::fact_agrees`]), which its
//! [`plumb_core::link_score`] then counts in ([`plumb_core::fact_trust`]).
//! Like `plumb link-rank`, it is run offline on copies: the records file
//! is left as it is.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result};
use clap::Args;
use plumb_core::article::articles_of;
use plumb_core::fact_check::{Check, FactBook};
use plumb_core::facts::{FactKind, KINDS};
use plumb_core::{canonical_domain, fact_accuracy, registrable_domain, Signals};

use crate::web::group_thousands;

/// Most checks one page adds: a list of a thousand birthdays is one
/// page's say, not a thousand.
const MAX_PAGE_CHECKS: usize = 50;

/// Characters of a sentence shown in an example.
const EXAMPLE_CHARS: usize = 220;

/// Where Common Crawl's files are, for the paths of a `wet.paths.gz`.
const COMMON_CRAWL: &str = "https://data.commoncrawl.org/";

/// A WET file to read: on disk, or to download first.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    File(PathBuf),
    Url(String),
}

impl Source {
    fn name(&self) -> String {
        match self {
            Source::File(path) => path.display().to_string(),
            Source::Url(url) => url.clone(),
        }
    }
}

/// `count` of the WET files a list names, spread evenly over it.
fn read_wet_list(path: &Path, count: usize) -> Result<Vec<Source>> {
    let reader = plumb_ingest::open_maybe_gz(path)?;
    let mut all = Vec::new();
    for line in reader.lines() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        all.push(
            if line.starts_with("https://") || line.starts_with("http://") {
                line.to_string()
            } else {
                format!("{COMMON_CRAWL}{}", line.trim_start_matches('/'))
            },
        );
    }
    let count = count.min(all.len());
    Ok((0..count)
        .map(|i| Source::Url(all[i * all.len() / count].clone()))
        .collect())
}

#[derive(Debug, Args)]
pub struct FactTrustArgs {
    /// English Wikipedia articles file with facts (`plumb fetch-facts`),
    /// most read first.
    #[arg(long, value_name = "FILE")]
    pub articles: PathBuf,
    /// Common Crawl WET files (`*.warc.wet.gz`) to read pages from.
    #[arg(long, value_name = "FILE", num_args = 1.., required_unless_present = "wet_list")]
    pub wet: Vec<PathBuf>,
    /// A list of WET files to download and read, one at a time per
    /// thread, each deleted once read: Common Crawl's `wet.paths.gz` of
    /// a crawl (paths under https://data.commoncrawl.org/), or URLs.
    #[arg(long, value_name = "FILE")]
    pub wet_list: Option<PathBuf>,
    /// With --wet-list, how many of its files to read, spread evenly
    /// over the list.
    #[arg(long, value_name = "N", default_value_t = 100, requires = "wet_list")]
    pub wet_files: usize,
    /// With --wet-list, where downloads are kept while they are read
    /// [default: next to --out, or the current directory].
    #[arg(long, value_name = "DIR", requires = "wet_list")]
    pub download_dir: Option<PathBuf>,
    /// Write each site's counts here, most checks first, as
    /// tab-separated lines.
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
    /// Records file (JSON lines) to copy with each site's counts, for
    /// --apply. A journal next to it is read too, from a copy.
    #[arg(long, value_name = "PATH", requires = "apply")]
    pub records: Option<PathBuf>,
    /// Write the copy of --records here.
    #[arg(long, value_name = "PATH", requires = "records")]
    pub apply: Option<PathBuf>,
    /// Statements shown that agree, and as many that do not, to see how
    /// well they are read.
    #[arg(long, value_name = "N", default_value_t = 25)]
    pub examples: usize,
    /// WET files read at once [default: one per CPU].
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,
}

/// A site's checked facts.
#[derive(Debug, Default)]
struct Tally {
    /// (article, kind, agrees) already counted.
    seen: HashSet<(u32, FactKind, bool)>,
    checks: u32,
    agrees: u32,
    pages: u32,
}

impl Tally {
    fn add(&mut self, check: Check) {
        if self.seen.insert((check.entity, check.kind, check.agrees)) {
            self.checks += 1;
            self.agrees += u32::from(check.agrees);
        }
    }

    fn absorb(&mut self, other: Tally) {
        self.pages += other.pages;
        for (entity, kind, agrees) in other.seen {
            self.add(Check {
                entity,
                kind,
                agrees,
            });
        }
    }

    fn signals(&self) -> Signals {
        Signals {
            fact_checks: self.checks,
            fact_agrees: self.agrees,
            ..Signals::default()
        }
    }
}

/// A statement shown as an example.
#[derive(Debug, Clone)]
struct Example {
    domain: String,
    check: Check,
    sentence: String,
}

/// What the WET files gave.
#[derive(Debug, Default)]
struct Found {
    sites: HashMap<String, Tally>,
    pages: u64,
    pages_checked: u64,
    /// Checks by kind, agreeing and not, before a site's repeats are
    /// taken out.
    by_kind: HashMap<FactKind, (u64, u64)>,
    agree_examples: Vec<Example>,
    disagree_examples: Vec<Example>,
}

impl Found {
    fn absorb(&mut self, other: Found, examples: usize) {
        self.pages += other.pages;
        self.pages_checked += other.pages_checked;
        for (kind, (yes, no)) in other.by_kind {
            let mine = self.by_kind.entry(kind).or_default();
            mine.0 += yes;
            mine.1 += no;
        }
        for (domain, tally) in other.sites {
            self.sites.entry(domain).or_default().absorb(tally);
        }
        for (mine, theirs) in [
            (&mut self.agree_examples, other.agree_examples),
            (&mut self.disagree_examples, other.disagree_examples),
        ] {
            let room = examples.saturating_sub(mine.len());
            mine.extend(theirs.into_iter().take(room));
        }
    }
}

/// Reads the articles file into a book of names and facts.
fn read_book(path: &Path) -> Result<FactBook> {
    let reader = plumb_ingest::open_maybe_gz(path)?;
    let mut failed = None;
    let lines = reader.lines().map_while(|line| match line {
        Ok(line) => Some(line),
        Err(err) => {
            failed = Some(err);
            None
        }
    });
    let mut book = FactBook::new();
    for (_, article) in articles_of(lines) {
        // A line that does not read is one article fewer.
        if let Ok(article) = article {
            book.add(&article);
        }
    }
    if let Some(err) = failed {
        return Err(anyhow::Error::new(err).context(format!("reading {}", path.display())));
    }
    Ok(book)
}

/// Downloads `url` to `path`, trying twice.
fn fetch(client: &reqwest::Client, url: &str, path: &Path) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting a runtime for downloads")?;
    let mut tries = 0;
    loop {
        tries += 1;
        match runtime.block_on(plumb_ingest::download::download_to_file(client, url, path)) {
            Ok(_) => return Ok(()),
            Err(err) if tries < 2 => eprintln!("{err:#}; trying again"),
            Err(err) => return Err(err),
        }
    }
}

/// Checks the pages of one WET file.
fn read_wet(book: &FactBook, path: &Path, examples: usize) -> Result<Found> {
    let mut found = Found::default();
    plumb_ingest::wet::for_each_wet_page(path, |url, text| {
        found.pages += 1;
        let Some(domain) = registrable_domain(url) else {
            return;
        };
        let mut page_checks = 0;
        let mut tally: Option<Tally> = None;
        for line in text.lines() {
            if page_checks >= MAX_PAGE_CHECKS {
                break;
            }
            let checks = book.check(line);
            for check in checks.into_iter().take(MAX_PAGE_CHECKS - page_checks) {
                page_checks += 1;
                let counts = found.by_kind.entry(check.kind).or_default();
                if check.agrees {
                    counts.0 += 1;
                } else {
                    counts.1 += 1;
                }
                let shown = if check.agrees {
                    &mut found.agree_examples
                } else {
                    &mut found.disagree_examples
                };
                // One example a site, so a few sites do not fill the list.
                if shown.len() < examples && !shown.iter().any(|e| e.domain == domain) {
                    shown.push(Example {
                        domain: domain.clone(),
                        check,
                        sentence: line.chars().take(EXAMPLE_CHARS * 4).collect(),
                    });
                }
                tally.get_or_insert_with(Tally::default).add(check);
            }
        }
        if let Some(mut tally) = tally {
            found.pages_checked += 1;
            tally.pages = 1;
            found.sites.entry(domain).or_default().absorb(tally);
        }
    })?;
    Ok(found)
}

pub fn run(args: &FactTrustArgs) -> Result<()> {
    let started = std::time::Instant::now();
    let book = read_book(&args.articles)?;
    println!(
        "{} articles, {} with facts to check ({:.0}s)",
        group_thousands(book.articles() as u64),
        group_thousands(book.entities() as u64),
        started.elapsed().as_secs_f64()
    );

    let mut sources: Vec<Source> = args.wet.iter().cloned().map(Source::File).collect();
    if let Some(list) = &args.wet_list {
        sources.extend(read_wet_list(list, args.wet_files)?);
    }
    let download_dir = args
        .download_dir
        .clone()
        .or_else(|| {
            args.out
                .as_ref()
                .and_then(|out| out.parent())
                .filter(|dir| !dir.as_os_str().is_empty())
                .map(Path::to_path_buf)
        })
        .unwrap_or_else(|| PathBuf::from("."));
    let client = if args.wet_list.is_some() {
        Some(plumb_ingest::download::http_client()?)
    } else {
        None
    };
    let threads = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
        .clamp(1, sources.len().max(1));
    let next = AtomicUsize::new(0);
    let all = Mutex::new(Found::default());
    let errors = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for worker in 0..threads {
            let (book, sources, next, all, errors) = (&book, &sources, &next, &all, &errors);
            let (client, download_dir) = (&client, &download_dir);
            scope.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(source) = sources.get(i) else {
                    break;
                };
                let read = match source {
                    Source::File(path) => read_wet(book, path, args.examples),
                    Source::Url(url) => {
                        let client = client.as_ref().expect("a client for downloads");
                        let path = download_dir.join(format!(".plumb-fact-trust-{worker}.wet.gz"));
                        let read = fetch(client, url, &path)
                            .and_then(|()| read_wet(book, &path, args.examples));
                        let _ = std::fs::remove_file(&path);
                        read
                    }
                };
                match read {
                    Ok(found) => {
                        let mut all = all.lock().expect("no thread panicked holding it");
                        all.absorb(found, args.examples);
                        eprintln!(
                            "read {} of {} WET files: {} pages, {} sites with checked facts",
                            i + 1,
                            sources.len(),
                            group_thousands(all.pages),
                            group_thousands(all.sites.len() as u64)
                        );
                    }
                    Err(err) => errors
                        .lock()
                        .expect("no thread panicked holding it")
                        .push(format!("{}: {err:#}", source.name())),
                }
            });
        }
    });
    let errors = errors.into_inner().expect("no thread panicked holding it");
    for error in &errors {
        eprintln!("skipped {error}");
    }
    let found = all.into_inner().expect("no thread panicked holding it");
    report(&book, &found, started);

    if let Some(out) = &args.out {
        write_sites(out, &found)?;
        println!(
            "\nwrote {} sites to {}",
            group_thousands(found.sites.len() as u64),
            out.display()
        );
    }
    if let (Some(records), Some(apply)) = (&args.records, &args.apply) {
        apply_to(records, apply, &found)?;
    }
    if !errors.is_empty() && errors.len() == sources.len() {
        anyhow::bail!("no WET file could be read");
    }
    Ok(())
}

fn report(book: &FactBook, found: &Found, started: std::time::Instant) {
    println!(
        "{} pages read, {} stating a fact checked ({:.2}%), {:.0}s",
        group_thousands(found.pages),
        group_thousands(found.pages_checked),
        100.0 * found.pages_checked as f64 / found.pages.max(1) as f64,
        started.elapsed().as_secs_f64()
    );
    println!("\nstatements by kind (a site's repeats included):");
    println!("  {:<20} {:>10} {:>8}", "kind", "checked", "agree");
    for kind in KINDS {
        if let Some(&(yes, no)) = found.by_kind.get(kind) {
            println!(
                "  {:<20} {:>10} {:>7.1}%",
                kind.key(),
                group_thousands(yes + no),
                100.0 * yes as f64 / (yes + no).max(1) as f64
            );
        }
    }
    let (checks, agrees) = found.sites.values().fold((0u64, 0u64), |(c, a), t| {
        (c + u64::from(t.checks), a + u64::from(t.agrees))
    });
    println!(
        "\n{} sites state checked facts: {} facts, {:.1}% agree",
        group_thousands(found.sites.len() as u64),
        group_thousands(checks),
        100.0 * agrees as f64 / checks.max(1) as f64
    );
    let scored: Vec<(&String, &Tally, f64)> = found
        .sites
        .iter()
        .filter_map(|(domain, tally)| fact_accuracy(&tally.signals()).map(|a| (domain, tally, a)))
        .collect();
    println!(
        "{} sites with at least {} facts get an accuracy",
        group_thousands(scored.len() as u64),
        plumb_core::MIN_FACT_CHECKS
    );
    let mut raw: Vec<f64> = scored
        .iter()
        .map(|(_, t, _)| f64::from(t.agrees) / f64::from(t.checks))
        .collect();
    raw.sort_by(f64::total_cmp);
    if !raw.is_empty() {
        let at = |q: f64| raw[((raw.len() - 1) as f64 * q) as usize];
        println!(
            "their share right: 10% {:.2}, 25% {:.2}, median {:.2}, 75% {:.2}, 90% {:.2}",
            at(0.1),
            at(0.25),
            at(0.5),
            at(0.75),
            at(0.9)
        );
    }
    let mut by_checks = scored.clone();
    by_checks.sort_by(|a, b| b.1.checks.cmp(&a.1.checks).then_with(|| a.0.cmp(b.0)));
    println!("\nmost checked sites (accuracy, right of checked, pages):");
    for (domain, tally, accuracy) in by_checks.iter().take(25) {
        println!(
            "  {accuracy:.3}  {:>6}/{:<6} {:>6}  {domain}",
            tally.agrees, tally.checks, tally.pages
        );
    }
    let mut worst = scored;
    worst.sort_by(|a, b| a.2.total_cmp(&b.2).then_with(|| a.0.cmp(b.0)));
    println!("\nleast accurate sites:");
    for (domain, tally, accuracy) in worst.iter().take(25) {
        println!(
            "  {accuracy:.3}  {:>6}/{:<6} {:>6}  {domain}",
            tally.agrees, tally.checks, tally.pages
        );
    }
    for (title, examples) in [
        ("statements that disagree", &found.disagree_examples),
        ("statements that agree", &found.agree_examples),
    ] {
        println!("\n{title} (site, article, kind, Wikidata's value):");
        for example in examples {
            let check = example.check;
            let truth: Vec<&str> = book.values(check.entity, check.kind).collect();
            println!(
                "  {}  {} / {} = {}\n      {}",
                example.domain,
                book.title(check.entity),
                check.kind.key(),
                truth.join(", "),
                around(&example.sentence, book.title(check.entity))
            );
        }
    }
}

/// Up to [`EXAMPLE_CHARS`] of `text` from a little before `name`.
fn around(text: &str, name: &str) -> String {
    let start = text.find(name).map_or(0, |at| at.saturating_sub(40));
    let start = (0..=start)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    text[start..].chars().take(EXAMPLE_CHARS).collect()
}

fn write_sites(path: &Path, found: &Found) -> Result<()> {
    let mut file =
        BufWriter::new(File::create(path).with_context(|| format!("writing {}", path.display()))?);
    let mut sites: Vec<(&String, &Tally)> = found.sites.iter().collect();
    sites.sort_by(|a, b| b.1.checks.cmp(&a.1.checks).then_with(|| a.0.cmp(b.0)));
    writeln!(file, "domain\tchecks\tagrees\tpages\taccuracy")?;
    for (domain, tally) in sites {
        let accuracy = fact_accuracy(&tally.signals()).map_or(String::new(), |a| format!("{a:.4}"));
        writeln!(
            file,
            "{domain}\t{}\t{}\t{}\t{accuracy}",
            tally.checks, tally.agrees, tally.pages
        )?;
    }
    file.flush()?;
    Ok(())
}

fn apply_to(records: &Path, apply: &Path, found: &Found) -> Result<()> {
    let dir = match records.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let copy = tempfile::Builder::new()
        .prefix(".plumb-fact-trust-")
        .suffix(".jsonl")
        .tempfile_in(dir)
        .with_context(|| format!("making a file in {}", dir.display()))?
        .into_temp_path();
    let read_from = match crate::outline::outline_copy(records, &copy)
        .with_context(|| format!("reading records {}", records.display()))?
    {
        Some((path, _)) => path.to_path_buf(),
        None => records.to_path_buf(),
    };
    let mut file = BufWriter::new(
        File::create(apply).with_context(|| format!("writing {}", apply.display()))?,
    );
    let (mut written, mut counted, mut scored) = (0u64, 0u64, 0u64);
    let mut failed = None;
    crate::outline::for_each_record(&read_from, |mut record| {
        if failed.is_some() {
            return;
        }
        written += 1;
        let tally = canonical_domain(&record.domain).and_then(|domain| found.sites.get(&domain));
        if let Some(tally) = tally {
            record.signals.fact_checks = tally.checks;
            record.signals.fact_agrees = tally.agrees;
            counted += 1;
            scored += u64::from(fact_accuracy(&record.signals).is_some());
        }
        let line = serde_json::to_writer(&mut file, &record)
            .map_err(anyhow::Error::from)
            .and_then(|()| file.write_all(b"\n").map_err(anyhow::Error::from));
        if let Err(err) = line {
            failed = Some(err);
        }
    })?;
    if let Some(err) = failed {
        return Err(err.context(format!("writing {}", apply.display())));
    }
    file.flush()?;
    println!(
        "\nwrote {} records to {}: {} with checked facts, {} of them with enough for an accuracy",
        group_thousands(written),
        apply.display(),
        group_thousands(counted),
        group_thousands(scored)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_site_counts_a_fact_once() {
        let check = |entity, agrees| Check {
            entity,
            kind: FactKind::Born,
            agrees,
        };
        let mut a = Tally::default();
        a.add(check(1, true));
        a.add(check(1, true));
        a.add(check(2, false));
        let mut b = Tally::default();
        b.add(check(1, true));
        b.add(check(3, true));
        a.absorb(b);
        assert_eq!((a.checks, a.agrees), (3, 2));
    }

    #[test]
    fn wet_lists_are_spread_and_made_urls() {
        let dir = tempfile::tempdir().unwrap();
        let list = dir.path().join("wet.paths");
        let lines: Vec<String> = (0..10)
            .map(|i| format!("crawl-data/x/{i}.warc.wet.gz"))
            .collect();
        std::fs::write(&list, lines.join("\n") + "\nhttps://example.org/a.wet.gz\n").unwrap();
        let picked = read_wet_list(&list, 3).unwrap();
        assert_eq!(
            picked,
            vec![
                Source::Url("https://data.commoncrawl.org/crawl-data/x/0.warc.wet.gz".into()),
                Source::Url("https://data.commoncrawl.org/crawl-data/x/3.warc.wet.gz".into()),
                Source::Url("https://data.commoncrawl.org/crawl-data/x/7.warc.wet.gz".into()),
            ]
        );
        assert_eq!(read_wet_list(&list, 100).unwrap().len(), 11);
        assert_eq!(
            read_wet_list(&list, 100).unwrap()[10],
            Source::Url("https://example.org/a.wet.gz".into())
        );
    }

    #[test]
    fn examples_show_the_name() {
        let text = format!("{}Albert Einstein was born in 1879.", "x".repeat(100));
        assert!(around(&text, "Albert Einstein").starts_with("xxxxxxxxxx"));
        assert!(around(&text, "Albert Einstein").contains("born in 1879"));
        assert_eq!(around("é Albert", "Albert"), "é Albert");
    }
}
