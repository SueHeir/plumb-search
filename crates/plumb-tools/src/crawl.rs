//! `plumb-tools crawl`: one crawl of a records file by hand, through the
//! same choice of homepages and saving as a node's rounds
//! ([`plumb_node::crawl`]). A batch that looks offline is fetched again
//! after 30 seconds, then 1, 2 and 4 minutes ([`OFFLINE_WAITS`]), since a
//! home router that was swamped usually recovers; when it still looks
//! offline, the crawl stops with an error.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Result};
use clap::Args;
use plumb_core::now_unix;
use plumb_crawl::{CrawlConfig, HomepageCrawler};
use plumb_node::cli::parse_positive;
use plumb_node::crawl::{
    crawl_rolling, select_targets, Fetcher, Rolling, RunEnd, CRAWL_BATCH_SIZE, SECONDS_PER_DAY,
};
use plumb_node::records::{load_records, RecordStore};
use tracing::info;

#[derive(Debug, Args)]
pub struct CrawlArgs {
    /// Records file to pick homepages from (JSON lines).
    #[arg(long, value_name = "PATH")]
    pub records: PathBuf,
    /// How many homepages to fetch: half from sites never tried and half from
    /// sites due again, best link score first in each (a half with too few
    /// leaves the rest to the other).
    #[arg(long, value_name = "N", default_value_t = 1000)]
    pub top: usize,
    /// Days before a homepage that was fetched, or answered with an error,
    /// is due again. Sites that could not be reached at all are retried
    /// after 1 day, then 2, 4, 8... days, up to this.
    #[arg(long, value_name = "D", default_value_t = 30)]
    pub skip_crawled_within_days: u64,
    /// Homepage fetches in flight at once.
    #[arg(long, value_name = "N", default_value_t = 16, value_parser = parse_positive)]
    pub concurrency: usize,
    /// Host name lookups in flight at once. Home routers drop lookups when
    /// hundreds arrive together; lower this if many sites come back as
    /// "could not be reached" at a high --concurrency.
    #[arg(long, value_name = "N", default_value_t = 32, value_parser = parse_positive)]
    pub dns_lookups: usize,
    /// Where to write the updated records [default: overwrite --records].
    /// Each batch of homepages is saved at once to a journal next to it
    /// (PATH.journal), which is folded in at the end, so an interrupted crawl
    /// keeps what it fetched; the next crawl or index of PATH picks it up.
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
    /// Fetch homepages through the proxy in HTTP_PROXY, HTTPS_PROXY or
    /// ALL_PROXY (except hosts in NO_PROXY), for machines that reach the
    /// internet only through one. Without it, homepages are fetched directly.
    #[arg(long)]
    pub use_system_proxy: bool,
}

/// How long a crawl waits before fetching a batch that looked offline
/// again, one wait per try; after the last, the crawl stops.
const OFFLINE_WAITS: [Duration; 4] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(240),
];

/// `plumb-tools crawl`: fetches the homepages of the best-scored records
/// and merges what they say into the records file.
pub fn run(args: CrawlArgs) -> Result<()> {
    let runtime = plumb_node::runtime()?;
    let cfg = CrawlConfig {
        concurrency: args.concurrency,
        dns_lookups: args.dns_lookups,
        use_system_proxy: args.use_system_proxy,
        ..CrawlConfig::default()
    };
    let mut fetcher = Rolling {
        crawler: HomepageCrawler::new(cfg),
        concurrency: args.concurrency,
        runtime: runtime.handle(),
    };
    crawl_file_with(&args, &OFFLINE_WAITS, &mut fetcher)
}

/// [`run`], with homepages fetched by `fetcher` and a batch that looks
/// offline fetched again after each of `offline_waits`.
fn crawl_file_with(
    args: &CrawlArgs,
    offline_waits: &[Duration],
    fetcher: &mut impl Fetcher,
) -> Result<()> {
    let mut set = load_records(&args.records)?;
    let out = args.out.as_deref().unwrap_or(&args.records);
    let mut store = RecordStore::open(out);
    if out != args.records {
        // The journal of `out` holds changes to `out`, which starts as what
        // was read.
        store.compact(&set)?;
    }
    let window = args
        .skip_crawled_within_days
        .saturating_mul(SECONDS_PER_DAY);
    let targets = select_targets(set.iter(), args.top, now_unix(), window);
    info!(
        "crawling {} of {} homepages, {} at a time",
        targets.len(),
        set.len(),
        args.concurrency
    );
    let totals = crawl_rolling(
        &mut set,
        &targets,
        CRAWL_BATCH_SIZE,
        &mut store,
        offline_waits,
        fetcher,
        |_| Ok(()),
    )?;
    let written = store.compact(&set)?;

    let o = &totals.outcomes;
    println!(
        "crawled {} homepages: {} fetched, {} blocked by robots.txt, {} errors",
        totals.attempted,
        o.fetched,
        o.robots_disallowed,
        o.errors()
    );
    if o.errors() > 0 {
        println!(
            "  errors: {} HTTP status, {} not HTML, {} bot checks, {} redirected off-site, \
             {} failed ({} could not be reached)",
            o.http_status, o.not_html, o.bot_check, o.offsite_redirect, o.failed, o.unreachable
        );
    }
    println!("discovered {} new domains", totals.discovered);
    println!("wrote {written} records to {}", out.display());
    if let RunEnd::Offline(offline) = totals.end {
        let proxy = if args.use_system_proxy {
            ""
        } else {
            " (if this machine reaches the internet through a proxy, add --use-system-proxy)"
        };
        bail!(
            "stopped crawling: {offline}. Check the network connection{proxy} and run the \
             crawl again"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use plumb_core::{write_jsonl, SiteRecord};
    use plumb_crawl::{CrawlOutcome, CrawlResult, CrawlTarget, CrawledPage, PageMeta};
    use plumb_node::records::{journal_path, Change};

    use super::*;

    /// A [`Fetcher`] that fetches each batch whole with a closure.
    struct Batches<F> {
        crawl: F,
        queued: Vec<CrawlTarget>,
    }

    impl<F: FnMut(Vec<CrawlTarget>) -> Vec<CrawlResult>> Fetcher for Batches<F> {
        fn start(&mut self, targets: Vec<CrawlTarget>) {
            self.queued.extend(targets);
        }

        fn finished(&mut self, _n: usize) -> Option<(Vec<String>, Vec<CrawlResult>)> {
            let batch = std::mem::take(&mut self.queued);
            let done = batch.iter().map(|target| target.domain.clone()).collect();
            Some((done, (self.crawl)(batch)))
        }
    }

    /// [`run`], with homepages fetched by `crawl` a batch at a time.
    fn crawl_file(
        args: &CrawlArgs,
        crawl: impl FnMut(Vec<CrawlTarget>) -> Vec<CrawlResult>,
    ) -> Result<()> {
        let mut fetcher = Batches {
            crawl,
            queued: Vec::new(),
        };
        crawl_file_with(args, &[], &mut fetcher)
    }

    /// Sites `site00.com`, `site01.com`... never tried, best first.
    fn sites(n: u32) -> Vec<SiteRecord> {
        (0..n)
            .map(|i| {
                let mut r = SiteRecord::new(format!("site{i:02}.com"));
                r.signals.tranco_rank = Some(i + 1);
                r
            })
            .collect()
    }

    fn fetched(domain: &str, fetched_at: u64) -> CrawlResult {
        CrawlResult {
            domain: domain.to_string(),
            outcome: CrawlOutcome::Fetched(CrawledPage {
                domain: domain.to_string(),
                final_url: format!("https://{domain}/"),
                status: 200,
                fetched_at,
                meta: PageMeta {
                    title: Some(format!("{domain} home")),
                    ..PageMeta::default()
                },
                icon: None,
            }),
        }
    }

    fn failed(domain: &str) -> CrawlResult {
        CrawlResult {
            domain: domain.to_string(),
            outcome: CrawlOutcome::Failed {
                error: "connection refused".into(),
                network: true,
            },
        }
    }

    fn crawl_args(records: &Path, out: Option<&Path>, top: usize) -> CrawlArgs {
        CrawlArgs {
            records: records.to_path_buf(),
            top,
            skip_crawled_within_days: 30,
            concurrency: 16,
            dns_lookups: 32,
            out: out.map(Path::to_path_buf),
            use_system_proxy: false,
        }
    }

    #[test]
    fn plumb_crawl_writes_the_whole_file_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        write_jsonl(&path, &sites(30)).unwrap();
        crawl_file(&crawl_args(&path, None, 10), |batch| {
            batch.iter().map(|t| fetched(&t.domain, 1)).collect()
        })
        .unwrap();
        assert!(!journal_path(&path).exists());
        let file: Vec<SiteRecord> = plumb_core::stored::read_file(&path).unwrap();
        assert_eq!(file.iter().filter(|r| r.crawled_at.is_some()).count(), 10);

        // To another file, which starts from the records read.
        let out = dir.path().join("out.jsonl");
        std::fs::write(journal_path(&out), "{\"op\":\"stale\"}\n").unwrap();
        crawl_file(&crawl_args(&path, Some(&out), 5), |batch| {
            batch.iter().map(|t| fetched(&t.domain, 2)).collect()
        })
        .unwrap();
        assert!(!journal_path(&out).exists());
        let copy: Vec<SiteRecord> = plumb_core::stored::read_file(&out).unwrap();
        assert_eq!(copy.iter().filter(|r| r.crawled_at.is_some()).count(), 15);
        let input: Vec<SiteRecord> = plumb_core::stored::read_file(&path).unwrap();
        assert_eq!(input, file, "the input is left alone");

        // An interrupted crawl's journal is picked up and folded in.
        let mut store = RecordStore::open(&path);
        let mut late = SiteRecord::new("late.com");
        late.title = Some("Saved before a crash".into());
        store
            .save(&[Change::Merge {
                site: late.stored_item_as_is(),
            }])
            .unwrap();
        drop(store);
        crawl_file(&crawl_args(&path, None, 0), |_| unreachable!()).unwrap();
        assert!(!journal_path(&path).exists());
        let file: Vec<SiteRecord> = plumb_core::stored::read_file(&path).unwrap();
        assert!(file.iter().any(|r| r.domain == "late.com"));
    }

    #[test]
    fn plumb_crawl_stops_when_the_network_is_down() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        write_jsonl(&path, &sites(30)).unwrap();
        let err = crawl_file(&crawl_args(&path, None, 30), |batch| {
            batch.iter().map(|t| failed(&t.domain)).collect()
        })
        .unwrap_err()
        .to_string();
        assert!(
            err.starts_with(
                "stopped crawling: 30 of 30 homepages that should have answered could not be \
                 fetched"
            ),
            "{err}"
        );
        assert!(err.contains("--use-system-proxy"), "{err}");
        // Nothing was marked, and the file is whole.
        assert!(!journal_path(&path).exists());
        let file: Vec<SiteRecord> = plumb_core::stored::read_file(&path).unwrap();
        assert_eq!(file, sites(30));

        let mut args = crawl_args(&path, None, 30);
        args.use_system_proxy = true;
        let err = crawl_file(&args, |batch| {
            batch.iter().map(|t| failed(&t.domain)).collect()
        })
        .unwrap_err()
        .to_string();
        assert!(!err.contains("--use-system-proxy"), "{err}");
    }
}
