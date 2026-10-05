//! Filling a node's free space with the network's crawls (Liz,
//! 2026-10-04: "nodes should have a way to request to fill their available
//! space with crawler data").
//!
//! A node takes in the crawls the network shares as they are made, but one
//! that just joined holds only the last few days of them, and the network
//! has crawled far more. So a node with room asks a node it trusts (see
//! [`crate::NetConfig::trusted_peers`]) for its crawled sites over
//! `/plumb/fill/1`, a stretch at a time, best-ranked first: the order its
//! [`crate::BucketTable`] keeps them in. The asker stops once its storage
//! budget is reached, so it keeps the best-known sites that fit.
//!
//! These records come from the answering node's own index, with no crawler
//! signature behind each one, so they are taken only from trusted nodes:
//! the operator vouches for what that node accepted. Every node trusts the
//! plumbsearch.org node unless told otherwise.
//!
//! A node setting up asks for every site of the list, crawled or not
//! (`all`), and needs no seed downloads (Tranco, Common Crawl, Wikidata,
//! Wikipedia): each record carries the ranks, names, Wikidata facts and
//! intro the trusted node's own seed gave it (Liz, 2026-10-04: "new nodes
//! don't need to pull from wiki or anywhere anymore, we have plenty of
//! nodes to serve the network now").
//!
//! A node answers at most [`MAX_FILLING`] fill requests at once and
//! [`FILL_REQUESTS_PER_MINUTE`] from one node, so a node filling up cannot
//! swamp a small server; it is told it is busy and asks again later.

use anyhow::{ensure, Context, Result};
use libp2p::PeerId;
use plumb_core::{canonical_domain, registrable_domain, search_link, SiteRecord};

use crate::assign::EPOCH_SECS;
use crate::batch::MAX_RECORD_BYTES;
use crate::bucket::BucketSource;
use crate::proto::FillResponse;

/// Most records sent for one fill request.
pub const MAX_FILL_RECORDS: u32 = 1_000;

/// Most records sent for one request for every site (`all`): those are
/// read straight off the list, so a page can be bigger.
pub const MAX_SEED_RECORDS: u32 = 5_000;

/// Most sites looked at for one fill request, crawled or not.
pub const MAX_FILL_SCAN: usize = 10_000;

/// Fill requests a node answers at once; more are told it is busy.
pub const MAX_FILLING: usize = 2;

/// Fill requests a node answers from one node a minute.
pub const FILL_REQUESTS_PER_MINUTE: u32 = 6;

/// What a trusted node sent of its crawled sites.
#[derive(Debug, Clone, PartialEq)]
pub struct FillPage {
    /// The node that answered.
    pub peer: PeerId,
    /// Its crawled sites, cut down by [`accept_filled`].
    pub records: Vec<SiteRecord>,
    /// Where to ask from next; `total` once its list is done.
    pub next: u64,
    /// Sites in its list, crawled or not.
    pub total: u64,
    /// It was busy and sent nothing; ask again later.
    pub busy: bool,
    /// Bytes of records received.
    pub bytes: u64,
}

impl FillPage {
    /// Whether the answering node's list is done.
    pub fn done(&self) -> bool {
        !self.busy && self.next >= self.total
    }
}

/// The answer to a fill request for `count` records from `from`: the
/// crawled sites among the next [`MAX_FILL_SCAN`] of the list, or with
/// `all` the next `count` sites, crawled or not.
pub fn answer(source: &dyn BucketSource, from: u64, count: u32, all: bool) -> FillResponse {
    let count = if all {
        count.min(MAX_SEED_RECORDS)
    } else {
        count.min(MAX_FILL_RECORDS)
    } as usize;
    let mut response = FillResponse {
        records: Vec::new(),
        next: from,
        total: 0,
        busy: false,
    };
    let Ok(start) = usize::try_from(from) else {
        return response;
    };
    let scan = if all { count } else { MAX_FILL_SCAN };
    let Some((lines, total)) = source.ranked(start, scan) else {
        return response;
    };
    response.total = total as u64;
    response.next = (start + lines.len()) as u64;
    for (i, line) in lines.into_iter().enumerate() {
        if response.records.len() == count {
            response.next = (start + i) as u64;
            break;
        }
        // Keys are never escaped in a record's JSON and string values
        // always are, so this is only ever the crawl time's key.
        if line.len() <= MAX_RECORD_BYTES && (all || line.contains("\"crawled_at\":")) {
            response.records.push(line);
        }
    }
    response
}

/// A crawled site a trusted node sent, as this node keeps it: its page
/// fields, names, link text and ranks, without the answering node's own
/// bookkeeping (crawl tries, redirects, icon). `None` for a record that
/// does not parse, is too long, names no registrable domain, or was never
/// crawled. Unsafe homepage URLs and site-search templates are stripped
/// without discarding the rest of the record.
pub fn accept_filled(line: &str, now: u64) -> Option<SiteRecord> {
    parse(line, now, false).ok()
}

/// As [`accept_filled`], for an answer to a request for every site
/// (`all`): a site never crawled is kept too, for its ranks and names.
pub fn accept_seed(line: &str, now: u64) -> Option<SiteRecord> {
    parse(line, now, true).ok()
}

fn parse(line: &str, now: u64, uncrawled: bool) -> Result<SiteRecord> {
    ensure!(line.len() <= MAX_RECORD_BYTES, "a record is too long");
    let mut record: SiteRecord = serde_json::from_str(line).context("a record does not parse")?;
    record.domain = canonical_domain(&record.domain).context("not a registrable domain")?;
    // A trusted node may hold old or malformed records. Its trust does not
    // allow a result to navigate to another site or run a non-web scheme.
    record.url = record.url.filter(|url| {
        let lower = url.to_ascii_lowercase();
        (lower.starts_with("https://") || lower.starts_with("http://"))
            && registrable_domain(url).as_deref() == Some(record.domain.as_str())
    });
    // Use the same validation as links generated from these templates,
    // including the known twitter.com -> x.com move.
    record.search_url = record
        .search_url
        .filter(|template| search_link(template, &record.domain, "x").is_some());
    record.key_pages = plumb_core::key_pages::valid_key_pages(
        std::mem::take(&mut record.key_pages),
        &record.domain,
    );
    match record.crawled_at {
        Some(crawled_at) => {
            ensure!(crawled_at <= now + EPOCH_SECS / 24, "crawled in the future");
        }
        None => ensure!(uncrawled, "never crawled"),
    }
    record.crawl_attempted_at = None;
    record.crawl_failures = 0;
    record.redirect = None;
    record.icon = None;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BucketTable;

    fn site(domain: &str, tranco: u32, crawled: bool) -> SiteRecord {
        let mut record = SiteRecord::new(domain);
        record.signals.tranco_rank = Some(tranco);
        if crawled {
            record.title = Some(format!("{domain} home"));
            record.crawled_at = Some(1_000);
        }
        record
    }

    #[test]
    fn answers_crawled_sites_best_ranked_first_a_stretch_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let records = vec![
            site("third.com", 3, true),
            site("first.com", 1, true),
            site("second.com", 2, false),
            site("fourth.com", 4, true),
        ];
        let table = BucketTable::build(&dir.path().join("b"), &records).unwrap();
        let ranked = table.ranked(1, 2).unwrap();
        assert!(ranked[0].contains("second.com") && ranked[1].contains("third.com"));
        assert!(table.ranked(4, 10).unwrap().is_empty());

        let first = answer(&table, 0, 2, false);
        assert_eq!(first.total, 4);
        let domains: Vec<String> = first
            .records
            .iter()
            .map(|line| accept_filled(line, 2_000).unwrap().domain)
            .collect();
        // second.com was never crawled, so it is skipped.
        assert_eq!(domains, ["first.com", "third.com"]);
        assert_eq!(first.next, 3);

        let rest = answer(&table, first.next, 2, false);
        assert_eq!(rest.records.len(), 1);
        assert!(rest.records[0].contains("fourth.com"));
        assert_eq!(rest.next, 4);
        assert_eq!(answer(&table, 4, 2, false).records.len(), 0);

        // A node setting up gets every site, crawled or not.
        let all = answer(&table, 0, 3, true);
        let domains: Vec<String> = all
            .records
            .iter()
            .map(|line| accept_seed(line, 2_000).unwrap().domain)
            .collect();
        assert_eq!(domains, ["first.com", "second.com", "third.com"]);
        assert_eq!((all.next, all.total), (3, 4));
        assert_eq!(answer(&table, all.next, 3, true).records.len(), 1);
    }

    #[test]
    fn a_node_without_a_table_sends_nothing() {
        struct Empty;
        impl BucketSource for Empty {
            fn bucket(&self, _: u32) -> Option<Vec<String>> {
                None
            }
        }
        let response = answer(&Empty, 0, 10, true);
        assert!(response.records.is_empty());
        assert_eq!((response.next, response.total), (0, 0));
    }

    #[test]
    fn keeps_what_a_crawl_says_but_not_the_senders_bookkeeping() {
        let mut record = site("Example.COM", 5, true);
        record.crawl_attempted_at = Some(1_500);
        record.crawl_failures = 2;
        record.icon = Some("png".into());
        let line = serde_json::to_string(&record).unwrap();
        let kept = accept_filled(&line, 2_000).unwrap();
        assert_eq!(kept.domain, "example.com");
        assert_eq!(kept.signals.tranco_rank, Some(5));
        assert_eq!(kept.title.as_deref(), Some("Example.COM home"));
        assert_eq!(
            (kept.crawl_attempted_at, kept.crawl_failures, kept.icon),
            (None, 0, None)
        );

        let uncrawled = serde_json::to_string(&site("a.com", 1, false)).unwrap();
        assert!(accept_filled(&uncrawled, 2_000).is_none());
        assert_eq!(accept_seed(&uncrawled, 2_000).unwrap().domain, "a.com");
        let mut future = site("b.com", 1, true);
        future.crawled_at = Some(1_000_000);
        let future = serde_json::to_string(&future).unwrap();
        assert!(accept_filled(&future, 2_000).is_none());
        assert!(accept_seed(&future, 2_000).is_none());
        assert!(accept_filled("not json", 2_000).is_none());
    }

    #[test]
    fn fill_and_seed_strip_unsafe_homepage_urls_without_losing_site_data() {
        for crawled in [true, false] {
            for url in [
                "https://attacker.com/steal",
                "https://example.com.attacker.com/",
                "https://example.com@attacker.com/",
                "javascript:alert(document.cookie)",
                "data:text/html,<script>alert(1)</script>",
                "file:///etc/passwd",
                "ftp://example.com/",
                "example.com/path",
                "//example.com/path",
                "https://127.0.0.1/",
                "https://",
            ] {
                let mut record = site("Example.COM", 5, crawled);
                record.url = Some(url.into());
                record.about = Some("A useful site".into());
                let line = serde_json::to_string(&record).unwrap();
                let kept = if crawled {
                    accept_filled(&line, 2_000)
                } else {
                    accept_seed(&line, 2_000)
                }
                .unwrap();
                assert!(
                    kept.url.is_none(),
                    "kept unsafe URL {url}, crawled={crawled}"
                );
                assert_eq!(kept.domain, "example.com");
                assert_eq!(kept.signals.tranco_rank, Some(5));
                assert_eq!(kept.about.as_deref(), Some("A useful site"));
                assert_eq!(kept.crawled_at, record.crawled_at);
            }
        }
    }

    #[test]
    fn fill_and_seed_keep_web_urls_on_the_canonical_registrable_domain() {
        for crawled in [true, false] {
            for (domain, url, expected_domain) in [
                ("Example.COM", "https://www.example.com/", "example.com"),
                ("example.com", "http://example.com/path", "example.com"),
                ("example.com", "HTTPS://shop.Example.COM/", "example.com"),
                (
                    "example.co.uk",
                    "https://www.example.co.uk/",
                    "example.co.uk",
                ),
                ("münchen.de", "https://www.münchen.de/", "xn--mnchen-3ya.de"),
            ] {
                let mut record = site(domain, 5, crawled);
                record.url = Some(url.into());
                let line = serde_json::to_string(&record).unwrap();
                let kept = if crawled {
                    accept_filled(&line, 2_000)
                } else {
                    accept_seed(&line, 2_000)
                }
                .unwrap();
                assert_eq!(kept.domain, expected_domain);
                assert_eq!(kept.url.as_deref(), Some(url));
            }
        }
    }

    #[test]
    fn fill_and_seed_validate_query_bearing_search_templates() {
        for crawled in [true, false] {
            for (domain, template, valid) in [
                (
                    "Example.COM",
                    "https://www.example.com/search?q={searchTerms}",
                    true,
                ),
                ("example.com", "http://example.com/find/{searchTerms}", true),
                ("twitter.com", "https://x.com/search?q={searchTerms}", true),
                (
                    "example.com",
                    "https://attacker.com/search?q={searchTerms}",
                    false,
                ),
                (
                    "example.com",
                    "https://example.com@attacker.com/?q={searchTerms}",
                    false,
                ),
                ("example.com", "javascript:alert('{searchTerms}')", false),
                ("example.com", "ftp://example.com/?q={searchTerms}", false),
                ("example.com", "example.com/?q={searchTerms}", false),
                ("example.com", "https://example.com/search", false),
                (
                    "example.com",
                    "https://example.com/{searchTerms}?q={searchTerms}",
                    false,
                ),
            ] {
                let mut record = site(domain, 5, crawled);
                record.search_url = Some(template.into());
                let line = serde_json::to_string(&record).unwrap();
                let kept = if crawled {
                    accept_filled(&line, 2_000)
                } else {
                    accept_seed(&line, 2_000)
                }
                .unwrap();
                assert_eq!(kept.search_url.as_deref(), valid.then_some(template));
                assert_eq!(kept.signals.tranco_rank, Some(5));
            }
        }
    }
}
