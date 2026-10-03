//! Turning crawl results into site records.

use std::collections::{BTreeMap, BTreeSet};

use plumb_core::{is_homepage_path, is_useful_anchor, linker_bit, registrable_domain, SiteRecord};
use url::Url;

use crate::{CrawlOutcome, CrawlResult};

/// Turns crawl results into records to merge into a
/// [`plumb_core::RecordSet`]: one record per fetched homepage (url, title,
/// description, `site_name` as an alias, `crawled_at`), plus one record per
/// linked domain carrying the texts of links to its front page and
/// `signals.linking_domains` = the number of distinct crawled domains
/// linking to it. Domains seen only as link targets are new discoveries.
///
/// Details:
/// - Only links to a front page ([`is_homepage_path`] on the URL's path,
///   so any query string is fine) contribute their text. Text on a deep
///   link names the page, not the site: an article headline, or "U.S. Bank"
///   on `facebook.com/usbank`, which would otherwise make facebook.com a
///   match for "us bank". Every link still counts toward `linking_domains`
///   and still creates the linked domain's record.
/// - A link text counts once per linking domain, however often that site
///   repeats it, and merging keeps it that way across crawls (see
///   [`plumb_core::LinkText::linkers`]). Links from a site to itself are
///   ignored, and so are texts that say nothing about the site ("click
///   here", "official website", the URL itself; see
///   [`plumb_core::is_useful_anchor`]).
/// - [`CrawlOutcome::OffsiteRedirect`] adds an empty record for the
///   registrable domain the redirect points to, so it gets discovered, and
///   nothing for the domain that redirected. Other outcomes add nothing.
/// - Records for the same domain are merged with [`SiteRecord::merge`], so
///   there is one record per domain, sorted by domain.
pub fn to_records(results: &[CrawlResult]) -> Vec<SiteRecord> {
    let mut records: BTreeMap<String, SiteRecord> = BTreeMap::new();
    // Linked domain -> linking (crawled) domain -> distinct link texts.
    let mut inbound: BTreeMap<&str, BTreeMap<&str, BTreeSet<&str>>> = BTreeMap::new();

    for result in results {
        match &result.outcome {
            CrawlOutcome::Fetched(page) => {
                let mut record = SiteRecord::new(page.domain.as_str());
                record.url = Some(page.final_url.clone());
                record.title = page.meta.title.clone();
                record.description = page.meta.description.clone();
                if let Some(site_name) = &page.meta.site_name {
                    record.add_alias(site_name);
                }
                record.crawled_at = Some(page.fetched_at);
                upsert(&mut records, record);

                for link in &page.meta.links {
                    if link.target_domain.is_empty() || link.target_domain == page.domain {
                        continue;
                    }
                    // Created even without a text: the link still counts.
                    let texts = inbound
                        .entry(link.target_domain.as_str())
                        .or_default()
                        .entry(page.domain.as_str())
                        .or_default();
                    if has_useful_front_page_text(&link.url, &link.text) {
                        texts.insert(link.text.as_str());
                    }
                }
            }
            CrawlOutcome::OffsiteRedirect { final_url } => {
                if let Some(domain) = registrable_domain(final_url) {
                    if domain != result.domain {
                        upsert(&mut records, SiteRecord::new(domain));
                    }
                }
            }
            CrawlOutcome::RobotsDisallowed
            | CrawlOutcome::HttpStatus { .. }
            | CrawlOutcome::NotHtml { .. }
            | CrawlOutcome::Failed { .. } => {}
        }
    }

    for (target, linking) in inbound {
        let mut record = SiteRecord::new(target);
        record.signals.linking_domains = u32::try_from(linking.len()).unwrap_or(u32::MAX);
        let mut text_linkers: BTreeMap<&str, u64> = BTreeMap::new();
        for (&linker, texts) in &linking {
            for &text in texts {
                *text_linkers.entry(text).or_default() |= linker_bit(linker);
            }
        }
        for (text, linkers) in text_linkers {
            // Ignores empty texts; keeps the most used ones.
            record.add_link_text_linkers(text, linkers);
        }
        upsert(&mut records, record);
    }

    records.into_values().collect()
}

/// Whether a link points at a site's front page (any query string is fine)
/// with text worth keeping.
fn has_useful_front_page_text(url: &str, text: &str) -> bool {
    Url::parse(url)
        .is_ok_and(|parsed| is_homepage_path(parsed.path()) && is_useful_anchor(text, url, &parsed))
}

fn upsert(records: &mut BTreeMap<String, SiteRecord>, record: SiteRecord) {
    match records.get_mut(&record.domain) {
        Some(existing) => existing.merge(record),
        None => {
            records.insert(record.domain.clone(), record);
        }
    }
}

#[cfg(test)]
mod tests {
    use plumb_core::{linker_bit, LinkText, RecordSet};

    use super::*;
    use crate::{CrawledPage, OutLink, PageMeta};

    /// A fetched homepage of `domain` with links given as (URL, normalized text).
    fn fetched(domain: &str, links: &[(&str, &str)]) -> CrawlResult {
        CrawlResult {
            domain: domain.into(),
            outcome: CrawlOutcome::Fetched(CrawledPage {
                domain: domain.into(),
                final_url: format!("https://www.{domain}/"),
                status: 200,
                fetched_at: 1_700_000_000,
                meta: PageMeta {
                    title: Some(format!("{domain} home")),
                    description: Some("About us".into()),
                    site_name: Some(format!("{domain} site")),
                    links: links
                        .iter()
                        .map(|&(url, text)| OutLink {
                            url: url.into(),
                            target_domain: registrable_domain(url).unwrap(),
                            text: text.into(),
                        })
                        .collect(),
                },
            }),
        }
    }

    fn result(domain: &str, outcome: CrawlOutcome) -> CrawlResult {
        CrawlResult {
            domain: domain.into(),
            outcome,
        }
    }

    fn texts(record: &SiteRecord) -> Vec<(&str, u32)> {
        record
            .link_texts
            .iter()
            .map(|lt| (lt.text.as_str(), lt.count))
            .collect()
    }

    #[test]
    fn fetched_pages_become_records() {
        let records = to_records(&[fetched("usbank.com", &[])]);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.domain, "usbank.com");
        assert_eq!(record.url.as_deref(), Some("https://www.usbank.com/"));
        assert_eq!(record.title.as_deref(), Some("usbank.com home"));
        assert_eq!(record.description.as_deref(), Some("About us"));
        assert_eq!(record.aliases, ["usbank.com site"]);
        assert_eq!(record.crawled_at, Some(1_700_000_000));
        assert!(record.link_texts.is_empty());
        assert_eq!(record.signals.linking_domains, 0);
    }

    #[test]
    fn links_become_link_texts_and_linking_domains() {
        let results = [
            fetched(
                "a.com",
                &[
                    ("https://target.org/", "target"),
                    ("https://www.target.org/", "target"),
                    ("https://target.org/", "our friends"),
                    ("https://b.com/", "bee"),
                    ("https://www.a.com/", "self link"),
                ],
            ),
            fetched(
                "b.com",
                &[
                    ("https://target.org/", "target"),
                    ("https://target.org/", ""),
                ],
            ),
            fetched("c.com", &[("https://target.org/", "")]),
        ];
        let records = to_records(&results);
        let domains: Vec<&str> = records.iter().map(|r| r.domain.as_str()).collect();
        assert_eq!(domains, ["a.com", "b.com", "c.com", "target.org"]);

        let target = &records[3];
        assert_eq!(texts(target), [("target", 2), ("our friends", 1)]);
        assert_eq!(target.signals.linking_domains, 3);
        assert_eq!((target.title.as_deref(), target.crawled_at), (None, None));

        // A crawled domain that is also linked keeps its page and gains texts.
        let b = &records[1];
        assert_eq!(b.title.as_deref(), Some("b.com home"));
        assert_eq!(texts(b), [("bee", 1)]);
        assert_eq!(b.signals.linking_domains, 1);

        // Self links do not count.
        let a = &records[0];
        assert!(a.link_texts.is_empty());
        assert_eq!(a.signals.linking_domains, 0);
    }

    #[test]
    fn only_links_to_front_pages_carry_text() {
        let results = [
            fetched(
                "usbank.com",
                &[
                    ("https://www.facebook.com/usbank", "us bank"),
                    (
                        "https://news.example.org/2026/10/03/story.html",
                        "big story",
                    ),
                    ("https://partner.org/?ref=usbank", "partner"),
                    ("https://www.partner.org/en-us/", "partner us"),
                    ("https://shop.example.net/index.html", "shop"),
                ],
            ),
            fetched("other.com", &[("https://www.facebook.com/", "facebook")]),
        ];
        let records = to_records(&results);
        let get = |domain: &str| records.iter().find(|r| r.domain == domain).unwrap();

        // Deep links still count and discover, but their text names the page.
        let facebook = get("facebook.com");
        assert_eq!(texts(facebook), [("facebook", 1)]);
        assert_eq!(facebook.signals.linking_domains, 2);
        let news = get("example.org");
        assert!(news.link_texts.is_empty());
        assert_eq!(news.signals.linking_domains, 1);

        // Front pages, with a query string, a locale or an index file.
        assert_eq!(
            texts(get("partner.org")),
            [("partner", 1), ("partner us", 1)]
        );
        assert_eq!(texts(get("example.net")), [("shop", 1)]);
    }

    #[test]
    fn texts_that_say_nothing_about_the_site_are_dropped() {
        let results = [fetched(
            "a.com",
            &[
                ("https://acme.example/", "click here"),
                ("https://acme.example/", "official website"),
                ("https://acme.example/", "home"),
                ("https://www.third.org/", "https www third org"),
                ("https://acme.example/", "acme rockets"),
            ],
        )];
        let records = to_records(&results);
        let get = |domain: &str| records.iter().find(|r| r.domain == domain).unwrap();
        assert_eq!(texts(get("acme.example")), [("acme rockets", 1)]);
        // The links still count.
        assert_eq!(get("acme.example").signals.linking_domains, 1);
        assert!(get("third.org").link_texts.is_empty());
        assert_eq!(get("third.org").signals.linking_domains, 1);
    }

    #[test]
    fn offsite_redirects_discover_the_destination_only() {
        let results = [
            result(
                "fb.com",
                CrawlOutcome::OffsiteRedirect {
                    final_url: "https://www.facebook.com/?_rdr".into(),
                },
            ),
            result(
                "ip.com",
                CrawlOutcome::OffsiteRedirect {
                    final_url: "http://10.0.0.1/".into(),
                },
            ),
        ];
        assert_eq!(to_records(&results), [SiteRecord::new("facebook.com")]);
    }

    #[test]
    fn other_outcomes_add_nothing() {
        let results = [
            result("a.com", CrawlOutcome::RobotsDisallowed),
            result("b.com", CrawlOutcome::HttpStatus { status: 503 }),
            result(
                "c.com",
                CrawlOutcome::NotHtml {
                    content_type: "application/pdf".into(),
                },
            ),
            result(
                "d.com",
                CrawlOutcome::Failed {
                    error: "timed out".into(),
                    network: true,
                },
            ),
        ];
        assert!(to_records(&results).is_empty());
    }

    #[test]
    fn records_merge_into_a_record_set() {
        let mut set = RecordSet::new();
        let mut known = SiteRecord::new("target.org");
        // Seen before from old.net and a.com; a.com links again below.
        known.add_link_text("target", "old.net");
        known.add_link_text("target", "a.com");
        known.signals.linking_domains = 1;
        set.upsert(known);
        set.extend(to_records(&[
            fetched("a.com", &[("https://target.org/", "target")]),
            fetched("b.com", &[("https://target.org/", "target")]),
        ]));
        let target = set.get("target.org").unwrap();
        assert_eq!(
            target.link_texts,
            [LinkText::from_linkers(
                "target",
                linker_bit("old.net") | linker_bit("a.com") | linker_bit("b.com")
            )]
        );
        assert_eq!(texts(target), [("target", 3)]);
        assert_eq!(target.signals.linking_domains, 2);
        assert_eq!(set.len(), 3);
    }
}
