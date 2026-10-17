//! A crawl round over a few made-up sites, checked against
//! `crawl_golden.txt` beside this file: what the round holds of each site
//! and picks, how it ends, the journal its batches write (read back), the
//! crawls it shares with the network (as the network carries them, and
//! how many bytes and lines they take), what the round holds after, and
//! the records file once the journal is folded in. Written by the round
//! that worked on records, before the crawl moved onto sites' items (step
//! 7b of the data format plan), so that the round on items is checked
//! against it byte for byte. Never written again to make a test pass:
//! `PLUMB_WRITE_GOLDEN=1 cargo test -p plumb-node --lib crawl_golden`
//! writes it.
//!
//! The sites differ in what a round reads (crawl marks, a site that looks
//! dead and one gone, names and titles that are not tidy, key pages, a
//! topic), and the crawl in what a crawl makes: homepages read, rich and
//! plain, sent elsewhere before and after the site they go to is read in
//! the same batch, links to front pages and deep ones, texts that say
//! nothing, a host no item can name, a key page at a URL that is not
//! canonical, icons, and failures. Times the crawl takes from the clock
//! are read back as [`NOW`].

use std::fmt::Write as _;
use std::path::Path;

use plumb_core::{KeyPage, SiteRecord};
use plumb_crawl::{CrawlOutcome, CrawlResult, CrawlTarget, CrawledPage, OutLink, PageMeta};

use crate::about::Topics;
use crate::crawl::{
    crawl_rolling, select_targets, select_targets_with, CrawlSet, CrawlSite, Fetcher,
};
use crate::node::round::RoundSites;
use crate::node::trim::Keep;
use crate::records::{journal_path, Change, RecordStore};

/// The round's clock, and what clock times the crawl takes read back as.
const NOW: u64 = 1_792_000_000;
const DAY: u64 = 24 * 60 * 60;
const WINDOW: u64 = 30 * DAY;

fn domain(i: u32) -> String {
    format!("s{i:02}.com")
}

/// The sites of the records file.
fn sites() -> Vec<SiteRecord> {
    (0..24).map(site).collect()
}

fn site(i: u32) -> SiteRecord {
    let mut r = SiteRecord::new(domain(i));
    r.signals.tranco_rank = Some(1 + i * 37 % 23 * 100);
    r.signals.linking_domains = i * 3 % 11;
    r.signals.official_site = i.is_multiple_of(7) || i == 16;
    match i % 6 {
        // Never tried.
        0 => {}
        // Read long ago, and due.
        1 => {
            r.crawled_at = Some(NOW - 40 * DAY);
            r.crawl_attempted_at = r.crawled_at;
            r.crawl_version = 3;
        }
        // Read lately.
        2 => {
            r.crawled_at = Some(NOW - 10 * DAY);
            r.crawl_attempted_at = r.crawled_at;
            r.crawl_version = 1;
        }
        // Unreachable a few times since it was read.
        3 => {
            r.crawled_at = Some(NOW - 50 * DAY);
            r.crawl_attempted_at = Some(NOW - u64::from(i % 4) * DAY);
            r.crawl_failures = 1 + i % 4;
            r.crawl_version = 2;
        }
        // Looks dead: never reached in many tries.
        4 => {
            r.crawl_attempted_at = Some(NOW - 5 * DAY);
            r.crawl_failures = 6 + i % 3;
        }
        // Gone, read years ago.
        _ => {
            r.crawled_at = Some(1_600_000_000 + u64::from(i));
            r.crawl_attempted_at = Some(NOW - 60 * DAY);
            r.crawl_failures = 7;
            r.gone_at = Some(NOW - 60 * DAY);
        }
    }
    if r.crawled_at.is_some() && r.gone_at.is_none() {
        r.url = Some(format!("https://www.{}/", domain(i)));
        r.title = Some(match i % 5 {
            0 => format!("S{i} Bank"),
            1 => format!("S{i} \u{1B}$B%3%_%C%H\u{1B}(B"),
            2 => format!(" S{i}\u{7} Shop "),
            3 => format!("S{i} Caf\u{e9} \u{FFFD}"),
            _ => format!("S{i} Chess Club"),
        });
        r.description = Some(format!("All about s{i}."));
        r.headings = vec!["News".into(), format!("Trust {i}")];
    }
    if i % 4 == 1 {
        r.key_pages = vec![KeyPage {
            label: "About".into(),
            url: format!("https://www.{}/about", domain(i)),
        }];
    }
    if i % 8 == 3 {
        r.aliases = vec![format!("S{i} Trust"), "Ste\u{301}phane".into()];
        r.kinds = vec!["bank".into()];
        r.about = Some("a bank".into());
        r.add_link_text("s three", "example.org");
    }
    if i == 20 {
        r.redirect = Some(plumb_core::Redirect {
            to: domain(21),
            at: NOW - 70 * DAY,
            inner: false,
        });
    }
    r
}

/// Writes `sites` as a records file, every third line as its stored item
/// and the rest as the record's JSON, as versions up to 0.4 wrote them.
fn write_file(path: &Path, sites: &[SiteRecord]) {
    let mut text = Vec::new();
    for (n, site) in sites.iter().enumerate() {
        let line = if n % 3 == 0 {
            plumb_core::stored::line(site).unwrap()
        } else {
            serde_json::to_vec(site).unwrap()
        };
        text.extend(line);
        text.push(b'\n');
    }
    std::fs::write(path, text).unwrap();
}

fn link(url: &str, text: &str) -> OutLink {
    OutLink {
        url: url.into(),
        target_domain: plumb_core::registrable_domain(url).unwrap_or_default(),
        text: text.into(),
    }
}

fn fetched(i: u32, meta: PageMeta, icon: Option<Vec<u8>>) -> CrawlOutcome {
    CrawlOutcome::Fetched(CrawledPage {
        domain: domain(i),
        final_url: format!("https://www.{}/", domain(i)),
        status: 200,
        fetched_at: NOW - 1_000 + u64::from(i),
        meta,
        icon,
    })
}

/// What the crawl finds of `target`, by the number of its site.
fn result_of(target: &CrawlTarget) -> CrawlResult {
    let i: u32 = target.domain[1..3].parse().unwrap();
    let outcome = match i % 5 {
        // Sent to the next site's front page, read in the same batch.
        1 => CrawlOutcome::OffsiteRedirect {
            final_url: format!("https://www.{}/?from=s{i}", domain(i + 1)),
        },
        // A rich homepage.
        2 => fetched(
            i,
            PageMeta {
                title: Some(format!("S{i} Bank & Trust")),
                description: Some(format!("Banking for s{i}.")),
                site_name: Some(if i == 7 {
                    "Cafe\u{301} Bank".into()
                } else {
                    format!("S{i} Bank")
                }),
                structured_names: vec![format!("S{i} Bank Inc."), format!("s{i} bank")],
                search_url: Some(format!("https://{}/search?q={{searchTerms}}", domain(i))),
                language: Some("en".into()),
                key_pages: vec![
                    KeyPage {
                        label: "Accounts".into(),
                        url: format!("https://www.{}/accounts", domain(i)),
                    },
                    KeyPage {
                        label: "Help".into(),
                        url: format!("HTTPS://WWW.S{i:02}.com/help%2ehtml"),
                    },
                ],
                headings: vec!["Personal".into(), "Business".into()],
                body_text: Some("Open an account today.".into()),
                terms: vec!["bank".into(), "accounts".into()],
                links: vec![
                    link(
                        &format!("https://{}/", domain(i + 2)),
                        &format!("S{i} partner"),
                    ),
                    link(&format!("https://www.{}/", domain(i + 2)), "partner"),
                    link("https://s03.com/news/1", "big story"),
                    link(&format!("https://www.{}/", domain(i)), "home page"),
                    link(&format!("https://new{i}.org/"), &format!("New {i}")),
                    link("https://foo_bar.com/", "foo bar"),
                    link(&format!("https://{}/?ref=x", domain(i - 1)), "click here"),
                ],
                ..PageMeta::default()
            },
            Some(vec![0x89, b'P', b'N', b'G', i as u8]),
        ),
        // Sent to a page inside the site before it, read in the same batch.
        3 => CrawlOutcome::OffsiteRedirect {
            final_url: if i == 13 {
                "http://10.0.0.1/".into()
            } else {
                format!("https://{}/en/about", domain(i - 1))
            },
        },
        // A plain homepage, with names and texts that are not tidy.
        4 => fetched(
            i,
            PageMeta {
                title: Some(if i == 9 {
                    "S09 \u{1B}$B%5%$%H\u{1B}(B".into()
                } else {
                    format!("S{i} Chess")
                }),
                site_name: Some(match i {
                    4 => "Ban\u{7}k Four".into(),
                    _ => format!("  S{i}   Chess  "),
                }),
                structured_names: vec!["Re\u{301}sume\u{301}".into(), "Résumé".into()],
                links: vec![
                    link(
                        &format!("https://{}/", domain(i - 2)),
                        &format!("S{i} bank"),
                    ),
                    link(
                        &format!("https://www.{}/", domain(i - 2)),
                        &format!("s{i} BANK"),
                    ),
                    link(&format!("https://www.{}/", domain(i + 1)), "next"),
                    link(&format!("https://new{}.org/", i - 2), "Ne\u{301}w"),
                    link("https://hub.org/", ""),
                    OutLink {
                        url: "https://nowhere/".into(),
                        target_domain: String::new(),
                        text: "nowhere".into(),
                    },
                ],
                ..PageMeta::default()
            },
            (i == 14).then(|| vec![7; 20_000]),
        ),
        _ => match i {
            5 => CrawlOutcome::Failed {
                error: "connection refused".into(),
                network: true,
            },
            10 => CrawlOutcome::RobotsDisallowed,
            _ => CrawlOutcome::HttpStatus { status: 503 },
        },
    };
    CrawlResult {
        domain: target.domain.clone(),
        outcome,
    }
}

/// Fetches each batch from [`result_of`], the second batch's results in
/// the other order, and notes what the node would share of each.
#[derive(Default)]
struct Crawl {
    queued: Vec<CrawlTarget>,
    batches: usize,
    shared: Vec<plumb_core::sites::SiteList>,
    /// The crawl's sites as it made them, before they were held as lines.
    made: Vec<Vec<plumb_format::Item>>,
}

impl Fetcher for Crawl {
    fn start(&mut self, targets: Vec<CrawlTarget>) {
        self.queued.extend(targets);
    }

    fn finished(&mut self, n: usize) -> Option<(Vec<String>, Vec<CrawlResult>)> {
        let batch: Vec<CrawlTarget> = self.queued.drain(..n.min(self.queued.len())).collect();
        let mut results: Vec<CrawlResult> = batch.iter().map(result_of).collect();
        if self.batches == 1 {
            results.reverse();
        }
        self.batches += 1;
        self.shared
            .push(crate::node::worker::published_crawl(&results));
        let mut made = plumb_crawl::to_items(&results);
        crate::icons::attach(&mut made, &results);
        self.made.push(made);
        Some((batch.into_iter().map(|t| t.domain).collect(), results))
    }
}

/// A time the crawl took from the clock, at `started` or after, as
/// [`NOW`].
fn steady(at: u64, started: u64) -> u64 {
    if at >= started {
        NOW
    } else {
        at
    }
}

fn steady_site(site: &mut plumb_format::Item, started: u64) {
    if let Some(mut redirect) = plumb_core::site::redirect(site) {
        redirect.at = steady(redirect.at, started);
        plumb_core::site::set_redirect(site, Some(redirect));
    }
}

/// `change` with the times the crawl took from the clock as [`NOW`].
fn steady_change(change: Change, started: u64) -> Change {
    match change {
        Change::Mark {
            domain,
            attempted_at,
            failures,
        } => Change::Mark {
            domain,
            attempted_at: attempted_at.map(|at| steady(at, started)),
            failures,
        },
        Change::Merge { mut site } => {
            if let Some(mut redirect) = plumb_core::site::redirect(&site) {
                redirect.at = steady(redirect.at, started);
                plumb_core::site::set_redirect(&mut site, Some(redirect));
            }
            Change::Merge { site }
        }
        change => change,
    }
}

fn round_sites(path: &Path) -> RoundSites {
    let focus = Topics::new(&["bank".to_string()]);
    let keep = Keep {
        topics: Topics::new(&["trust".to_string()]),
        domains: [domain(10)].into(),
    };
    RoundSites::load(path, focus, keep).unwrap()
}

#[test]
fn a_crawl_round_of_the_golden_sites_makes_the_golden_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records.jsonl");
    write_file(&path, &sites());
    let original = std::fs::read(&path).unwrap();
    let mut text = String::from(
        "# A crawl round over the made-up sites of crawl_golden.rs: what it\n\
         # holds of each site and picks, how it ends, its journal read back,\n\
         # what it shares (each site as the network carries it, then the\n\
         # bytes and lines of each batch), what it holds after, and the\n\
         # records file with the journal folded in. Written by the round on\n\
         # records; see crawl_golden.rs before writing it again.\n",
    );

    let mut set = round_sites(&path);
    for site in set.iter() {
        writeln!(text, "round {site:?}").unwrap();
    }
    let focused = select_targets(set.iter().filter(|s| s.focused()), 3, NOW, WINDOW / 2);
    let rest = select_targets_with(
        set.iter()
            .filter(|s| !focused.iter().any(|t| t.domain == s.domain())),
        8,
        NOW,
        WINDOW,
        |s| s.misread() || s.crawl_version() < 1 || !s.has_key_pages(),
    );
    for (kind, targets) in [("focused", &focused), ("rest", &rest)] {
        for target in targets {
            let known = target.known_url.as_deref().unwrap_or("-");
            writeln!(text, "pick {kind} {} {known}", target.domain).unwrap();
        }
    }

    let targets: Vec<CrawlTarget> = (1..=15).map(|i| CrawlTarget::new(&domain(i))).collect();
    let started = plumb_core::now_unix();
    let mut store = RecordStore::open(&path);
    let mut crawl = Crawl::default();
    let totals = crawl_rolling(&mut set, &targets, 5, &mut store, &[], &mut crawl, |_| {
        Ok(())
    })
    .unwrap();
    writeln!(text, "totals {totals:?}").unwrap();

    let journal = std::fs::read_to_string(journal_path(&path)).unwrap();
    let changes: Vec<Change> = journal
        .lines()
        .map(|line| steady_change(serde_json::from_str(line).unwrap(), started))
        .collect();
    let mut lines = String::new();
    for change in &changes {
        let line = serde_json::to_string(change).unwrap();
        writeln!(text, "journal {line}").unwrap();
        lines.push_str(&line);
        lines.push('\n');
    }

    for (batch, (sites, mut made)) in crawl.shared.into_iter().zip(crawl.made).enumerate() {
        let mut sites: Vec<plumb_format::Item> = sites.iter().collect();
        for site in &mut sites {
            steady_site(site, started);
            let line = plumb_core::site::to_line(site).unwrap_or_else(|err| format!("({err})"));
            writeln!(text, "shared {batch} {line}").unwrap();
        }
        // The size of the batch as the records' JSON of the crawl's sites
        // as it made them, which the room to publish it was measured by
        // before the network took items. Held as lines, a site with a
        // link no item can name is cleaned already, as it is when signed.
        for site in &mut made {
            steady_site(site, started);
        }
        let records: Vec<SiteRecord> = (made.iter())
            .map(|site| SiteRecord::from_stored_item(site).unwrap())
            .collect();
        let bytes = serde_json::to_vec(&records).unwrap().len();
        let lines: usize = sites
            .iter()
            .map(|site| {
                1 + usize::from(plumb_core::site::icon(site).is_some())
                    + usize::from(!plumb_core::site::news(site).is_empty())
            })
            .sum();
        writeln!(text, "shared {batch} bytes {bytes} lines {lines}").unwrap();
    }

    let again = dir.path().join("again.jsonl");
    std::fs::write(&again, &original).unwrap();
    let mut after = round_sites(&again);
    for change in changes {
        after.apply(change);
    }
    for site in after.iter() {
        writeln!(text, "after {site:?}").unwrap();
    }

    let folded = dir.path().join("folded.jsonl");
    std::fs::write(&folded, &original).unwrap();
    std::fs::write(journal_path(&folded), lines).unwrap();
    RecordStore::open(&folded).fold().unwrap();
    for line in std::fs::read_to_string(&folded).unwrap().lines() {
        writeln!(text, "folded {line}").unwrap();
    }

    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/node/tests/crawl_golden.txt");
    crate::outline::tests::check_golden(&golden, &text);
}
