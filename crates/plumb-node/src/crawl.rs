//! `plumb crawl`: refreshes the homepages of the best-scored records and
//! merges what they say, including links that discover new domains.
//!
//! Homepages are fetched [`CRAWL_BATCH_SIZE`] at a time, and the records file
//! is saved after every batch, so an interrupted crawl keeps what it fetched.
//! Every homepage tried gets `crawl_attempted_at`, whatever the outcome, so
//! sites that keep failing wait out the same window as fetched ones instead
//! of taking the top of every run.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use plumb_core::{now_unix, read_jsonl, RecordSet, SiteRecord};
use plumb_crawl::{
    crawl_homepages, to_records, CrawlConfig, CrawlOutcome, CrawlResult, CrawlTarget,
};
use tracing::info;

use crate::cli::CrawlArgs;
use crate::{runtime, write_records_atomically};

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// Homepages fetched between two saves of the records file.
const CRAWL_BATCH_SIZE: usize = 500;

pub fn run(args: CrawlArgs) -> Result<()> {
    let records: Vec<SiteRecord> = read_jsonl(&args.records)
        .with_context(|| format!("loading records {}", args.records.display()))?;
    let mut set: RecordSet = records.into_iter().collect();

    let cutoff = now_unix().saturating_sub(
        args.skip_crawled_within_days
            .saturating_mul(SECONDS_PER_DAY),
    );
    let targets = select_targets(set.iter(), args.top, cutoff);
    info!(
        "crawling {} of {} homepages, {} at a time, saving every {CRAWL_BATCH_SIZE}",
        targets.len(),
        set.len(),
        args.concurrency
    );

    let out = args.out.as_ref().unwrap_or(&args.records);
    let runtime = runtime()?;
    let cfg = CrawlConfig {
        concurrency: args.concurrency,
        ..CrawlConfig::default()
    };
    let totals = crawl_in_batches(&mut set, &targets, CRAWL_BATCH_SIZE, out, |batch| {
        runtime.block_on(crawl_homepages(batch, &cfg))
    })?;

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
            "  errors: {} HTTP status, {} not HTML, {} redirected off-site, {} failed to connect or read",
            o.http_status, o.not_html, o.offsite_redirect, o.failed
        );
    }
    println!("discovered {} new domains", totals.discovered);
    println!("wrote {} records to {}", totals.written, out.display());
    Ok(())
}

/// The homepages to fetch: records whose homepage was neither fetched nor
/// tried since `cutoff` (Unix seconds), best link score first (ties by
/// domain), at most `top`.
fn select_targets<'a>(
    records: impl Iterator<Item = &'a SiteRecord>,
    top: usize,
    cutoff: u64,
) -> Vec<CrawlTarget> {
    let mut due: Vec<(f32, &str)> = records
        .filter(|r| last_visit(r).is_none_or(|t| t < cutoff))
        .map(|r| (r.link_score(), r.domain.as_str()))
        .collect();
    due.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    due.into_iter()
        .take(top)
        .map(|(_, domain)| CrawlTarget::homepage(domain))
        .collect()
}

/// When the homepage was last fetched or tried, whichever is later.
fn last_visit(record: &SiteRecord) -> Option<u64> {
    record.crawled_at.max(record.crawl_attempted_at)
}

/// What a whole crawl run did, over all its batches.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RunTotals {
    /// Homepages tried.
    attempted: usize,
    outcomes: CrawlSummary,
    /// Domains that were not in the records file before the run.
    discovered: usize,
    /// Records in the file at the last save.
    written: usize,
}

/// Fetches `targets` with `crawl`, `batch_size` at a time. After each batch
/// the results are merged into `set` and every record is saved to `out`,
/// best link score first. With no targets, `set` is saved once as it is.
fn crawl_in_batches(
    set: &mut RecordSet,
    targets: &[CrawlTarget],
    batch_size: usize,
    out: &Path,
    mut crawl: impl FnMut(Vec<CrawlTarget>) -> Vec<CrawlResult>,
) -> Result<RunTotals> {
    let batch_size = batch_size.max(1);
    let batches = targets.len().div_ceil(batch_size);
    let mut merger = BatchMerger::default();
    let mut totals = RunTotals::default();
    for (i, batch) in targets.chunks(batch_size).enumerate() {
        let attempted_at = now_unix();
        let results = crawl(batch.to_vec());
        let outcomes = CrawlSummary::of(&results);
        totals.attempted += batch.len();
        totals.outcomes.add(&outcomes);
        totals.discovered += merger.merge(set, batch, &results, attempted_at);
        totals.written = write_records_atomically(out, sorted_by_link_score(set))?;
        info!(
            "batch {}/{batches}: fetched {} of {} homepages; saved {} records to {}",
            i + 1,
            outcomes.fetched,
            batch.len(),
            totals.written,
            out.display()
        );
    }
    if targets.is_empty() {
        totals.written = write_records_atomically(out, sorted_by_link_score(set))?;
    }
    Ok(totals)
}

/// Folds crawl batches into a record set.
#[derive(Debug, Default)]
struct BatchMerger {
    /// Distinct crawled domains linking to each domain, over the whole run.
    /// [`to_records`] counts the linking domains of one batch only, and
    /// merging keeps the larger of two counts, so without this a site linked
    /// from several batches would keep only the count of its best batch.
    linkers: HashMap<String, HashSet<String>>,
}

impl BatchMerger {
    /// Folds one batch into `set`: the records [`to_records`] makes from the
    /// results, run-wide `linking_domains` for the domains they link to, and
    /// `crawl_attempted_at` on every target's record, whatever its outcome.
    /// Returns how many domains were new to `set`.
    fn merge(
        &mut self,
        set: &mut RecordSet,
        targets: &[CrawlTarget],
        results: &[CrawlResult],
        attempted_at: u64,
    ) -> usize {
        let mut discovered = 0;
        for record in to_records(results) {
            if set.get(&record.domain).is_none() {
                discovered += 1;
            }
            set.upsert(record);
        }

        let mut linked: HashSet<&str> = HashSet::new();
        for result in results {
            let CrawlOutcome::Fetched(page) = &result.outcome else {
                continue;
            };
            for link in &page.meta.links {
                // The links `to_records` counts: self-links are not inbound.
                if link.target_domain.is_empty() || link.target_domain == page.domain {
                    continue;
                }
                self.linkers
                    .entry(link.target_domain.clone())
                    .or_default()
                    .insert(page.domain.clone());
                linked.insert(&link.target_domain);
            }
        }
        for domain in linked {
            let count = self
                .linkers
                .get(domain)
                .map_or(0, |l| u32::try_from(l.len()).unwrap_or(u32::MAX));
            let signals = &mut set.entry(domain).signals;
            signals.linking_domains = signals.linking_domains.max(count);
        }

        for target in targets {
            let record = set.entry(&target.domain);
            // Like `SiteRecord::merge`, keep the later time.
            record.crawl_attempted_at = record.crawl_attempted_at.max(Some(attempted_at));
        }
        discovered
    }
}

/// All records in the order [`RecordSet::into_sorted_vec`] gives (best link
/// score first, ties by domain), without copying them.
fn sorted_by_link_score(set: &RecordSet) -> Vec<&SiteRecord> {
    let mut scored: Vec<(f32, &SiteRecord)> = set.iter().map(|r| (r.link_score(), r)).collect();
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.domain.cmp(&b.1.domain))
    });
    scored.into_iter().map(|(_, r)| r).collect()
}

/// Crawl outcomes counted by kind.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CrawlSummary {
    fetched: usize,
    robots_disallowed: usize,
    http_status: usize,
    not_html: usize,
    offsite_redirect: usize,
    failed: usize,
}

impl CrawlSummary {
    fn of(results: &[CrawlResult]) -> Self {
        let mut s = CrawlSummary::default();
        for result in results {
            match &result.outcome {
                CrawlOutcome::Fetched(_) => s.fetched += 1,
                CrawlOutcome::RobotsDisallowed => s.robots_disallowed += 1,
                CrawlOutcome::HttpStatus { .. } => s.http_status += 1,
                CrawlOutcome::NotHtml { .. } => s.not_html += 1,
                CrawlOutcome::OffsiteRedirect { .. } => s.offsite_redirect += 1,
                CrawlOutcome::Failed { .. } => s.failed += 1,
            }
        }
        s
    }

    fn add(&mut self, other: &CrawlSummary) {
        self.fetched += other.fetched;
        self.robots_disallowed += other.robots_disallowed;
        self.http_status += other.http_status;
        self.not_html += other.not_html;
        self.offsite_redirect += other.offsite_redirect;
        self.failed += other.failed;
    }

    /// Everything that was neither fetched nor blocked by robots.txt.
    fn errors(&self) -> usize {
        self.http_status + self.not_html + self.offsite_redirect + self.failed
    }
}

#[cfg(test)]
mod tests {
    use plumb_crawl::{CrawledPage, OutLink, PageMeta};

    use super::*;

    fn record(
        domain: &str,
        tranco: u32,
        crawled_at: Option<u64>,
        attempted_at: Option<u64>,
    ) -> SiteRecord {
        let mut r = SiteRecord::new(domain);
        r.signals.tranco_rank = Some(tranco);
        r.crawled_at = crawled_at;
        r.crawl_attempted_at = attempted_at;
        r
    }

    fn domains(targets: &[CrawlTarget]) -> Vec<&str> {
        targets.iter().map(|t| t.domain.as_str()).collect()
    }

    /// A fetched homepage of `domain` linking to the front page of each of `links_to`.
    fn fetched(domain: &str, fetched_at: u64, links_to: &[&str]) -> CrawlResult {
        let links = links_to
            .iter()
            .map(|target| OutLink {
                url: format!("https://{target}/"),
                target_domain: target.to_string(),
                text: "linked site".to_string(),
            })
            .collect();
        CrawlResult {
            domain: domain.to_string(),
            outcome: CrawlOutcome::Fetched(CrawledPage {
                domain: domain.to_string(),
                final_url: format!("https://{domain}/"),
                status: 200,
                fetched_at,
                meta: PageMeta {
                    title: Some(format!("{domain} home")),
                    links,
                    ..PageMeta::default()
                },
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

    #[test]
    fn picks_best_scored_records_not_visited_recently() {
        let records = [
            record("c.com", 30, None, None),          // never tried: due
            record("a.com", 10, Some(1_000), None),   // fetched after the cutoff: skipped
            record("b.com", 20, Some(10), None),      // fetched long ago: due again
            record("f.com", 5, None, Some(900)),      // failed after the cutoff: skipped
            record("g.com", 15, Some(10), Some(800)), // tried again after the cutoff: skipped
            record("h.com", 25, None, Some(50)),      // failed long ago: due again
            record("d.com", 40, None, None),
            record("e.com", 40, None, None), // ties broken by domain
        ];
        let targets = select_targets(records.iter(), 3, 500);
        assert_eq!(domains(&targets), ["b.com", "h.com", "c.com"]);
        assert_eq!(targets[0], CrawlTarget::homepage("b.com"));

        let all = select_targets(records.iter(), 100, 500);
        assert_eq!(domains(&all), ["b.com", "h.com", "c.com", "d.com", "e.com"]);
        assert!(select_targets(records.iter(), 0, 500).is_empty());
    }

    #[test]
    fn merging_a_batch_stamps_every_target() {
        let mut set: RecordSet = [
            record("a.com", 10, None, None),
            record("b.com", 20, Some(100), Some(100)),
            record("idle.com", 30, None, None),
        ]
        .into_iter()
        .collect();
        let targets = [
            CrawlTarget::homepage("a.com"),
            CrawlTarget::homepage("b.com"),
        ];
        let results = [fetched("a.com", 2_010, &["new.com"]), failed("b.com")];
        let discovered = BatchMerger::default().merge(&mut set, &targets, &results, 2_000);
        assert_eq!(discovered, 1);

        let a = set.get("a.com").unwrap();
        assert_eq!(
            (a.crawled_at, a.crawl_attempted_at),
            (Some(2_010), Some(2_000))
        );
        assert_eq!(a.title.as_deref(), Some("a.com home"));
        let b = set.get("b.com").unwrap();
        assert_eq!(
            (b.crawled_at, b.crawl_attempted_at),
            (Some(100), Some(2_000))
        );
        assert_eq!(set.get("idle.com").unwrap().crawl_attempted_at, None);
        let new = set.get("new.com").unwrap();
        assert_eq!((new.crawled_at, new.crawl_attempted_at), (None, None));
        assert_eq!(new.signals.linking_domains, 1);

        // Both targets now wait out the window, the failed one included.
        let next = select_targets(set.iter(), 10, 1_500);
        assert_eq!(domains(&next), ["idle.com", "new.com"]);
    }

    #[test]
    fn linking_domains_count_over_the_whole_run() {
        let results = [
            fetched("a.com", 1, &["hub.com", "a.com"]),
            fetched("b.com", 1, &["hub.com"]),
            fetched("c.com", 1, &["hub.com", "other.com"]),
        ];
        let unbatched: RecordSet = to_records(&results).into_iter().collect();
        assert_eq!(unbatched.get("hub.com").unwrap().signals.linking_domains, 3);

        let mut set = RecordSet::new();
        let mut merger = BatchMerger::default();
        for batch in results.chunks(2) {
            merger.merge(&mut set, &[], batch, 1);
        }
        for domain in ["hub.com", "other.com"] {
            assert_eq!(
                set.get(domain).unwrap().signals,
                unbatched.get(domain).unwrap().signals,
                "{domain}"
            );
        }

        // A larger count from another source (a WAT file, say) is kept.
        let mut known = SiteRecord::new("hub.com");
        known.signals.linking_domains = 10;
        let mut set: RecordSet = [known].into_iter().collect();
        BatchMerger::default().merge(&mut set, &[], &results, 1);
        assert_eq!(set.get("hub.com").unwrap().signals.linking_domains, 10);
    }

    #[test]
    fn saves_after_every_batch_and_totals_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("records.jsonl");
        let mut set: RecordSet = ["a.com", "b.com", "c.com", "d.com", "e.com"]
            .iter()
            .zip([10, 20, 30, 40, 50])
            .map(|(domain, tranco)| record(domain, tranco, None, None))
            .collect();
        let targets = select_targets(set.iter(), 10, 0);
        let started = now_unix();

        let mut batch_sizes: Vec<usize> = Vec::new();
        let totals = crawl_in_batches(&mut set, &targets, 2, &out, |batch| {
            // Whatever earlier batches tried is already on disk.
            if !batch_sizes.is_empty() {
                let saved: Vec<SiteRecord> = read_jsonl(&out).unwrap();
                let tried = saved
                    .iter()
                    .filter(|r| r.crawl_attempted_at.is_some())
                    .count();
                assert_eq!(tried, batch_sizes.iter().sum::<usize>());
            }
            batch_sizes.push(batch.len());
            batch
                .iter()
                .map(|t| match t.domain.as_str() {
                    "b.com" => failed("b.com"),
                    domain => fetched(domain, started, &["linked.com"]),
                })
                .collect()
        })
        .unwrap();

        assert_eq!(batch_sizes, [2, 2, 1]);
        // Totals cover all three batches, and linked.com is new only once.
        let outcomes = CrawlSummary {
            fetched: 4,
            failed: 1,
            ..CrawlSummary::default()
        };
        assert_eq!(
            totals,
            RunTotals {
                attempted: 5,
                outcomes,
                discovered: 1,
                written: 6,
            }
        );

        let saved: Vec<SiteRecord> = read_jsonl(&out).unwrap();
        let order: Vec<&str> = saved.iter().map(|r| r.domain.as_str()).collect();
        assert_eq!(
            order,
            ["a.com", "b.com", "c.com", "d.com", "e.com", "linked.com"]
        );
        for r in &saved[..5] {
            assert!(r.crawl_attempted_at.is_some_and(|t| t >= started), "{r:?}");
        }
        assert_eq!(saved[1].crawled_at, None);
        assert_eq!(saved[5].signals.linking_domains, 4);
        assert_eq!(saved[5].crawl_attempted_at, None);
    }

    #[test]
    fn saves_once_when_there_is_nothing_to_crawl() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("copy.jsonl");
        let mut set: RecordSet = [record("a.com", 10, Some(5), None)].into_iter().collect();
        let totals = crawl_in_batches(&mut set, &[], 500, &out, |_| {
            panic!("nothing should be crawled")
        })
        .unwrap();
        assert_eq!(
            totals,
            RunTotals {
                written: 1,
                ..RunTotals::default()
            }
        );
        let saved: Vec<SiteRecord> = read_jsonl(&out).unwrap();
        assert_eq!(saved, vec![record("a.com", 10, Some(5), None)]);
    }

    #[test]
    fn sorts_like_record_set() {
        let set: RecordSet = [
            record("b.com", 50, None, None),
            record("a.com", 50, None, None),
            record("c.com", 5, None, None),
            SiteRecord::new("z.com"),
        ]
        .into_iter()
        .collect();
        let by_ref: Vec<&str> = sorted_by_link_score(&set)
            .iter()
            .map(|r| r.domain.as_str())
            .collect();
        let owned: Vec<String> = set
            .clone()
            .into_sorted_vec()
            .into_iter()
            .map(|r| r.domain)
            .collect();
        assert_eq!(by_ref, owned);
        assert_eq!(by_ref, ["c.com", "a.com", "b.com", "z.com"]);
    }

    #[test]
    fn counts_outcomes() {
        let result = |domain: &str, outcome| CrawlResult {
            domain: domain.to_string(),
            outcome,
        };
        let page = CrawledPage {
            domain: "a.com".into(),
            final_url: "https://a.com/".into(),
            status: 200,
            fetched_at: 1,
            meta: PageMeta::default(),
        };
        let results = [
            result("a.com", CrawlOutcome::Fetched(page)),
            result("b.com", CrawlOutcome::RobotsDisallowed),
            result("c.com", CrawlOutcome::HttpStatus { status: 503 }),
            result(
                "d.com",
                CrawlOutcome::NotHtml {
                    content_type: "application/pdf".into(),
                },
            ),
            result(
                "e.com",
                CrawlOutcome::OffsiteRedirect {
                    final_url: "https://f.com/".into(),
                },
            ),
            result(
                "g.com",
                CrawlOutcome::Failed {
                    error: "dns".into(),
                    network: true,
                },
            ),
            result(
                "h.com",
                CrawlOutcome::Failed {
                    error: "timeout".into(),
                    network: true,
                },
            ),
        ];
        let summary = CrawlSummary::of(&results);
        assert_eq!(
            summary,
            CrawlSummary {
                fetched: 1,
                robots_disallowed: 1,
                http_status: 1,
                not_html: 1,
                offsite_redirect: 1,
                failed: 2,
            }
        );
        assert_eq!(summary.errors(), 5);
    }
}
