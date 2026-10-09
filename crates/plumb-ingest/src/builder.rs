//! Folds every seed source into one set of [`SiteRecord`]s.

use std::cmp::Ordering;
use std::collections::{hash_map::Entry, HashMap, HashSet};

use plumb_core::{
    canonical_domain, kind_key, linker_count, parent_domain, subdomain_sites, RecordSet,
    SiteRecord, MAX_LINK_TEXTS, SUBDOMAIN_SITE_NAMES,
};
use tracing::{info, warn};

use crate::{CcRank, OfficialSite, TrancoEntry, WatExtract};

/// More distinct Wikidata items than this claiming one domain's front page
/// mark the domain as a shared host rather than anyone's official site.
const MAX_ITEMS_PER_HOMEPAGE: usize = 5;
/// How many times the sitelinks of every other item claiming a front page
/// an item needs for its facts alone to be the site's.
const DOMINANT_SITELINKS: u32 = 3;

/// Collects seed data into site records. Each `add_*` call merges into the
/// records already collected, so sources can be added in any order.
#[derive(Debug, Default)]
pub struct Builder {
    records: RecordSet,
}

impl Builder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct domains collected so far.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Sets `signals.tranco_rank`.
    ///
    /// A domain that already has a rank keeps the better (lower) one.
    /// Domains are made canonical first ([`plumb_core::canonical_domain`]),
    /// and entries whose domain is not a valid registrable domain, or whose
    /// rank is 0, are ignored.
    pub fn add_tranco(&mut self, entries: &[TrancoEntry]) {
        let mut invalid = 0u64;
        for entry in entries {
            if entry.rank == 0 {
                continue;
            }
            let Some(domain) = canonical_domain(&entry.domain) else {
                invalid += 1;
                continue;
            };
            let signals = &mut self.records.entry(&domain).signals;
            signals.tranco_rank = best_rank(signals.tranco_rank, Some(entry.rank));
        }
        warn_invalid("Tranco entries", invalid);
    }

    /// Sets `signals.harmonic_rank` and `signals.pagerank_rank`.
    ///
    /// A domain that already has ranks keeps the better (lower) ones.
    /// Domains are made canonical first, and entries whose domain is not a
    /// valid registrable domain, or whose harmonic rank is 0, are ignored.
    pub fn add_cc_ranks(&mut self, ranks: &[CcRank]) {
        let mut invalid = 0u64;
        for rank in ranks {
            if rank.harmonic_rank == 0 {
                continue;
            }
            let Some(domain) = canonical_domain(&rank.domain) else {
                invalid += 1;
                continue;
            };
            let signals = &mut self.records.entry(&domain).signals;
            signals.harmonic_rank = best_rank(signals.harmonic_rank, Some(rank.harmonic_rank));
            signals.pagerank_rank =
                best_rank(signals.pagerank_rank, rank.pagerank_rank.filter(|&r| r > 0));
        }
        warn_invalid("Common Crawl ranks", invalid);
    }

    /// Adds homepage title/description/url (and `og:site_name` as an alias),
    /// inbound link texts, and `signals.linking_domains`. Target domains that
    /// only appear as link targets get new records, which is how WAT data
    /// discovers sites missing from the rank lists.
    ///
    /// Link texts come only from links to a site's front page (see
    /// [`WatExtract::add_document`]). WAT pages carry no crawl time, so they
    /// only fill page fields that are still empty (see [`SiteRecord::merge`]);
    /// link text counts add up, and `linking_domains` keeps the larger count.
    pub fn add_wat(&mut self, extract: &WatExtract) {
        let domains: HashSet<&String> = extract
            .homepages
            .keys()
            .chain(extract.anchors.keys())
            .chain(extract.linking_domains.keys())
            .collect();
        for domain in domains {
            if domain.is_empty() {
                continue;
            }
            let mut record = SiteRecord::new(domain.as_str());
            if let Some(meta) = extract.homepages.get(domain) {
                record.url = Some(meta.url.clone());
                record.title = meta.title.clone();
                record.description = meta.description.clone();
                if let Some(site_name) = &meta.site_name {
                    record.add_alias(site_name);
                }
            }
            if let Some(texts) = extract.anchors.get(domain) {
                // Only the most used texts can survive the cap, so skip
                // re-sorting the record's list for all the others.
                let mut texts: Vec<(&String, u64, u32)> = texts
                    .iter()
                    .map(|(text, &linkers)| (text, linkers, linker_count(linkers)))
                    .collect();
                texts.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(b.0)));
                for (text, linkers, _) in texts.into_iter().take(MAX_LINK_TEXTS) {
                    record.add_link_text_linkers(text, linkers);
                }
            }
            if let Some(linkers) = extract.linking_domains.get(domain) {
                record.signals.linking_domains = u32::try_from(linkers.len()).unwrap_or(u32::MAX);
            }
            self.records.upsert(record);
        }
    }

    /// Marks the official websites listed in Wikidata: sets
    /// `signals.official_site`, adds the item's label and other names as
    /// aliases and, when
    /// the record has none yet, sets its `country`, and adds its kinds (see
    /// [`crate::attach_facts`]). When several items claim a front page, an
    /// item with [`DOMINANT_SITELINKS`] times the sitelinks of each other
    /// one gives the facts (wikipedia.org is Wikipedia's, an online
    /// encyclopedia, more than its community's); else only the country and
    /// kinds they all share count: x.com is both X Corp.'s and the old
    /// X.com bank's, and is no bank.
    ///
    /// Only a claim on the domain's own front page counts (see
    /// [`OfficialSite::is_root_homepage`]): `https://www.ox.ac.uk/` makes
    /// `ox.ac.uk` the University of Oxford's site, while a college at
    /// `https://www.balliol.ox.ac.uk/` or a band at
    /// `https://linktr.ee/acmerockets` neither grants nor takes away anything
    /// for the parent domain. One exception: an inner page of a domain no
    /// item claims the front page of counts when the item's names name the
    /// domain ([`OfficialSite::is_named_inner_page`]), so Perplexity AI at
    /// `https://www.perplexity.ai/hub/` makes perplexity.ai its site, while
    /// the film Rocky at a bit.ly link does not. A front page claimed by more than five
    /// different items is a shared host rather than anyone's official site,
    /// so it is skipped entirely.
    ///
    /// A label that is just the item id (what Wikidata's label service
    /// returns for items without an English label) is not added as an alias.
    pub fn add_official_sites(&mut self, sites: &[OfficialSite]) {
        // Front page claims by domain, in input order, and named inner page
        // claims, which count for a domain without front page claims.
        let mut claims: HashMap<String, Vec<&OfficialSite>> = HashMap::new();
        let mut named_inner: HashMap<String, Vec<&OfficialSite>> = HashMap::new();
        let mut inner = 0u64;
        let mut invalid = 0u64;
        for site in sites {
            let front_page = site.is_root_homepage();
            if !front_page && !site.is_named_inner_page() {
                inner += 1;
                continue;
            }
            let Some(domain) = canonical_domain(&site.domain) else {
                invalid += 1;
                continue;
            };
            if front_page {
                claims.entry(domain).or_default().push(site);
            } else {
                named_inner.entry(domain).or_default().push(site);
            }
        }
        let mut by_inner_page = 0usize;
        for (domain, sites) in named_inner {
            if let Entry::Vacant(entry) = claims.entry(domain) {
                entry.insert(sites);
                by_inner_page += 1;
            } else {
                inner += sites.len() as u64;
            }
        }
        let (mut official, mut shared) = (0usize, 0usize);
        for (domain, claims) in &claims {
            let items: HashSet<&str> = claims.iter().map(|site| site.item.as_str()).collect();
            if items.len() > MAX_ITEMS_PER_HOMEPAGE {
                shared += 1;
                continue;
            }
            official += 1;
            let record = self.records.entry(domain);
            record.signals.official_site = true;
            let sitelinks = claims.iter().map(|site| site.sitelinks).max().unwrap_or(0);
            record.signals.sitelinks = record.signals.sitelinks.max(sitelinks);
            for site in claims {
                let label = site.label.trim();
                if label != site.item {
                    record.add_alias(label);
                }
            }
            // Then their other names, after every main name.
            for site in claims {
                for name in &site.names {
                    record.add_alias(name);
                }
            }
            // When several items claim the front page, the facts are those
            // of the item that is far better known than the others
            // (Wikipedia over the Wikipedia community at wikipedia.org);
            // without one, only what they agree on is the site's: their
            // shared country and kinds (x.com's former bank and X Corp.).
            // (Claims of one item carry the same facts.)
            let dominant = claims.iter().copied().find(|site| {
                site.sitelinks > 0
                    && claims.iter().all(|other| {
                        other.item == site.item
                            || site.sitelinks >= DOMINANT_SITELINKS.saturating_mul(other.sitelinks)
                    })
            });
            let (first, others): (&OfficialSite, &[&OfficialSite]) = match dominant {
                Some(site) => (site, &[]),
                None => {
                    let (first, others) = claims.split_first().expect("a domain has a claim");
                    (first, others)
                }
            };
            let mut country = first.country.clone();
            let mut kinds: Vec<String> = first.kinds.clone();
            for site in others {
                if country != site.country {
                    country = None;
                }
                kinds.retain(|kind| {
                    site.kinds
                        .iter()
                        .any(|other| kind_key(other) == kind_key(kind))
                });
            }
            if record.country.is_none() {
                record.country = country;
            }
            // A description only says what the site is when one item claims
            // it, or one is far better known.
            if items.len() == 1 || dominant.is_some() {
                if record.about.is_none() {
                    record.about.clone_from(&first.about);
                }
                if record.intro.is_none() {
                    record.intro.clone_from(&first.intro);
                }
            }
            for kind in &kinds {
                record.add_kind(kind);
            }
        }
        warn_invalid("Wikidata official sites", invalid);
        info!(
            "marked {official} official sites ({by_inner_page} by an inner page naming the site); skipped {shared} front pages claimed by more than {MAX_ITEMS_PER_HOMEPAGE} Wikidata items, and {inner} claims on a subdomain or an inner page"
        );
    }

    /// Merges ready-made records, e.g. from a previous run or a crawl.
    pub fn add_records<I: IntoIterator<Item = SiteRecord>>(&mut self, records: I) {
        self.records.extend(records);
    }

    /// All records, best link score first (ties broken by domain), cut to
    /// the best `top_n` when given.
    ///
    /// With a cut, each record is scored once, only the kept ones are
    /// sorted, and they are moved out of the set rather than copied.
    pub fn finish(mut self, top_n: Option<usize>) -> Vec<SiteRecord> {
        self.rank_subdomain_sites();
        let total = self.records.len();
        let n = match top_n {
            Some(n) if n < total => n,
            _ => return self.records.into_sorted_vec(),
        };
        fn best_first(a: &(f32, &SiteRecord), b: &(f32, &SiteRecord)) -> Ordering {
            b.0.total_cmp(&a.0)
                .then_with(|| a.1.domain.cmp(&b.1.domain))
        }
        let mut scored: Vec<(f32, &SiteRecord)> = self
            .records
            .iter()
            .map(|record| (record.link_score(), record))
            .collect();
        scored.select_nth_unstable_by(n, best_first);
        scored.truncate(n);
        scored.sort_unstable_by(best_first);
        let kept: Vec<String> = scored
            .into_iter()
            .map(|(_, record)| record.domain.clone())
            .collect();
        info!("kept the top {n} of {total} records by link score");
        kept.iter()
            .map(|domain| std::mem::take(self.records.entry(domain)))
            .collect()
    }
}

impl Builder {
    /// Gives each subdomain that is a site of its own
    /// ([`plumb_core::subdomain_sites`]) and has a record, such as
    /// news.ycombinator.com from Wikidata, the ranks of its domain that it
    /// lacks: the rank lists only rank registrable domains. Subdomain sites
    /// with no record are not made, so they take no room under a cap, but
    /// for the few the seed data may not name ([`SUBDOMAIN_SITE_NAMES`]).
    fn rank_subdomain_sites(&mut self) {
        for (site, name) in SUBDOMAIN_SITE_NAMES {
            let parent = parent_domain(site).and_then(|domain| self.records.get(domain));
            if parent.is_some() && self.records.get(site).is_none() {
                let record = self.records.entry(site);
                record.signals.official_site = true;
                record.add_alias(name);
            }
        }
        for (site, domain) in subdomain_sites() {
            let (Some(_), Some(parent)) = (self.records.get(site), self.records.get(domain)) else {
                continue;
            };
            let parent = parent.signals.clone();
            let signals = &mut self.records.entry(site).signals;
            signals.tranco_rank = signals.tranco_rank.or(parent.tranco_rank);
            signals.harmonic_rank = signals.harmonic_rank.or(parent.harmonic_rank);
            signals.pagerank_rank = signals.pagerank_rank.or(parent.pagerank_rank);
        }
    }
}

/// Logs seed entries a `Builder::add_*` call dropped for an invalid domain.
/// The loaders never produce them, so these come from hand-built input.
fn warn_invalid(source: &str, invalid: u64) {
    if invalid > 0 {
        warn!("ignored {invalid} {source} without a valid registrable domain");
    }
}

/// The better (lower) of two optional 1-based ranks.
fn best_rank<T: Ord>(current: Option<T>, new: Option<T>) -> Option<T> {
    match (current, new) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use plumb_core::{linker_bit, LinkText, Signals, MAX_LINKER_ESTIMATE};

    use super::*;
    use crate::{
        load_cc_domain_ranks, load_tranco, load_wikidata_official_sites, parse_wat, WatPage,
        WatWriter,
    };

    fn linking(url: &str, links: &[(&str, &str)]) -> WatPage {
        WatPage {
            url: url.to_string(),
            status: 200,
            links: links
                .iter()
                .map(|(href, text)| (href.to_string(), text.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn write_wat(path: &Path, pages: &[WatPage]) {
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut writer = WatWriter::new(file, true, "seed.warc.wat.gz").unwrap();
        for page in pages {
            writer.write_page(page).unwrap();
        }
        writer.finish().unwrap();
    }

    /// A claim on the front page of `domain`.
    fn site(item: &str, label: &str, domain: &str) -> OfficialSite {
        OfficialSite::new(item, label, &format!("https://{domain}/")).unwrap()
    }

    /// Adds `(item, label, url)` claims to a new builder and returns every
    /// record it made as `(domain, aliases)`, sorted. Each must be official:
    /// claims that do not count create no record.
    fn official(claims: &[(&str, &str, &str)]) -> Vec<(String, Vec<String>)> {
        let sites: Vec<OfficialSite> = claims
            .iter()
            .map(|(item, label, url)| OfficialSite::new(*item, *label, url).unwrap())
            .collect();
        let mut builder = Builder::new();
        builder.add_official_sites(&sites);
        let records = builder.finish(None);
        assert!(records.iter().all(|r| r.signals.official_site));
        let mut out: Vec<(String, Vec<String>)> =
            records.into_iter().map(|r| (r.domain, r.aliases)).collect();
        out.sort();
        out
    }

    fn official_with(domain: &str, aliases: &[&str]) -> (String, Vec<String>) {
        (
            domain.to_string(),
            aliases.iter().map(|a| a.to_string()).collect(),
        )
    }

    #[test]
    fn combines_all_four_sources() {
        let dir = tempfile::tempdir().unwrap();
        let tranco = dir.path().join("tranco-top-1m.csv");
        std::fs::write(&tranco, "1,google.com\n2,facebook.com\n3,www.usbank.com\n").unwrap();

        let ranks = dir.path().join("domain-ranks.txt");
        std::fs::write(
            &ranks,
            "#harmonicc_pos\t#harmonicc_val\t#pr_pos\t#pr_val\t#host_rev\t#n_hosts\n\
             1\t3.0E7\t2\t0.01\tcom.google\t100\n\
             7\t2.0E7\t9\t0.001\tcom.usbank\t12\n",
        )
        .unwrap();

        let wat = dir.path().join("seed.warc.wat.gz");
        write_wat(
            &wat,
            &[
                WatPage {
                    title: Some("U.S. Bank | Personal Banking".into()),
                    description: Some("Checking, savings and credit cards".into()),
                    site_name: Some("U.S. Bank".into()),
                    ..linking(
                        "https://www.usbank.com/",
                        &[("https://www.facebook.com/usbank", "U.S. Bank")],
                    )
                },
                linking(
                    "https://a.com/",
                    &[("https://www.usbank.com/", "U.S. Bank")],
                ),
                linking(
                    "https://b.org/x",
                    &[
                        ("https://usbank.com/", "US Bank"),
                        ("https://usbank.com/", "Click here"),
                    ],
                ),
                linking("https://c.net/", &[("https://newsite.io/", "New Site")]),
            ],
        );

        let wikidata = dir.path().join("wikidata-official-sites.tsv");
        let mut tsv = String::from(
            "item\tlabel\twebsite\n\
             http://www.wikidata.org/entity/Q739868\tU.S. Bancorp\thttps://www.usbank.com/\n\
             Q12345\tQ12345\thttps://usbank.com\n\
             Q95\tGoogle\thttps://www.google.com/\n",
        );
        for i in 1..=6 {
            tsv.push_str(&format!(
                "Q{i}0\tBrand {i}\thttps://www.facebook.com/brand{i}\n"
            ));
        }
        std::fs::write(&wikidata, tsv).unwrap();

        let mut builder = Builder::new();
        builder.add_tranco(&load_tranco(&tranco, None).unwrap());
        builder.add_cc_ranks(&load_cc_domain_ranks(&ranks, None).unwrap());
        let mut extract = WatExtract::new();
        parse_wat(&wat, &mut extract).unwrap();
        builder.add_wat(&extract);
        builder.add_official_sites(&load_wikidata_official_sites(&wikidata).unwrap());

        // google, facebook, usbank, plus a.com, c.net (homepages) and newsite.io (link target).
        assert_eq!(builder.len(), 6);
        let records = builder.finish(None);
        let usbank = records.iter().find(|r| r.domain == "usbank.com").unwrap();
        assert_eq!(usbank.url.as_deref(), Some("https://www.usbank.com/"));
        assert_eq!(
            usbank.title.as_deref(),
            Some("U.S. Bank | Personal Banking")
        );
        assert_eq!(
            usbank.description.as_deref(),
            Some("Checking, savings and credit cards")
        );
        assert_eq!(usbank.aliases, ["U.S. Bank", "U.S. Bancorp"]);
        assert_eq!(
            usbank.link_texts,
            [LinkText::from_linkers(
                "us bank",
                linker_bit("a.com") | linker_bit("b.org")
            )]
        );
        assert_eq!(usbank.link_texts[0].count, 2);
        assert_eq!(
            usbank.signals,
            Signals {
                harmonic_rank: Some(7),
                pagerank_rank: Some(9),
                tranco_rank: Some(3),
                linking_domains: 2,
                official_site: true,
                sitelinks: 0,
                ..Signals::default()
            }
        );
        assert_eq!(usbank.crawled_at, None);

        // Six items claim pages on facebook.com, none its front page, so it is
        // not anyone's official site.
        let facebook = records.iter().find(|r| r.domain == "facebook.com").unwrap();
        assert!(!facebook.signals.official_site);
        assert!(facebook.aliases.is_empty());
        // The link to facebook.com/usbank counts, but its "U.S. Bank" text names
        // the profile page, so facebook.com gets no link text for it.
        assert_eq!(facebook.signals.linking_domains, 1);
        assert!(facebook.link_texts.is_empty());

        // A domain seen only as a link target still gets a record.
        let newsite = records.iter().find(|r| r.domain == "newsite.io").unwrap();
        assert_eq!(newsite.title, None);
        assert_eq!(newsite.link_texts[0].text, "new site");

        assert_eq!(records[0].domain, "google.com");
        assert!(records[0].signals.official_site);
    }

    #[test]
    fn ranks_keep_the_best_value() {
        let mut builder = Builder::new();
        builder.add_tranco(&[
            TrancoEntry {
                rank: 50,
                domain: "a.com".into(),
            },
            TrancoEntry {
                rank: 0,
                domain: "zero.com".into(),
            },
            TrancoEntry {
                rank: 5,
                domain: String::new(),
            },
        ]);
        builder.add_tranco(&[TrancoEntry {
            rank: 70,
            domain: "a.com".into(),
        }]);
        builder.add_cc_ranks(&[
            CcRank {
                domain: "a.com".into(),
                harmonic_rank: 900,
                pagerank_rank: None,
                n_hosts: Some(3),
            },
            CcRank {
                domain: "a.com".into(),
                harmonic_rank: 1000,
                pagerank_rank: Some(400),
                n_hosts: None,
            },
        ]);
        assert_eq!(builder.len(), 1);
        let records = builder.finish(None);
        assert_eq!(
            records[0].signals,
            Signals {
                tranco_rank: Some(50),
                harmonic_rank: Some(900),
                pagerank_rank: Some(400),
                ..Default::default()
            }
        );
    }

    #[test]
    fn subdomain_sites_get_their_domains_ranks_and_their_own_names() {
        let mut builder = Builder::new();
        builder.add_tranco(&[
            TrancoEntry {
                rank: 900,
                domain: "ycombinator.com".into(),
            },
            TrancoEntry {
                rank: 1,
                domain: "google.com".into(),
            },
        ]);
        builder.add_cc_ranks(&[CcRank {
            domain: "ycombinator.com".into(),
            harmonic_rank: 300,
            pagerank_rank: Some(200),
            n_hosts: None,
        }]);
        builder.add_official_sites(&[
            site("Q1", "Y Combinator", "www.ycombinator.com"),
            site("Q2", "Hacker News", "news.ycombinator.com"),
        ]);
        let records = builder.finish(None);
        let get = |domain: &str| records.iter().find(|r| r.domain == domain).unwrap();
        let (yc, news) = (get("ycombinator.com"), get("news.ycombinator.com"));
        assert_eq!(news.signals.tranco_rank, Some(900));
        assert_eq!(news.signals.harmonic_rank, Some(300));
        assert_eq!(news.signals.pagerank_rank, Some(200));
        assert_eq!(news.aliases, ["Hacker News"]);
        assert_eq!(yc.aliases, ["Y Combinator"]);
        // Google's products with no record of their own are not made.
        assert_eq!(records.len(), 3);
    }

    #[test]
    fn hacker_news_gets_a_record_without_wikidata() {
        let mut builder = Builder::new();
        builder.add_tranco(&[TrancoEntry {
            rank: 900,
            domain: "ycombinator.com".into(),
        }]);
        let records = builder.finish(None);
        let news = records
            .iter()
            .find(|r| r.domain == "news.ycombinator.com")
            .unwrap();
        assert_eq!(news.aliases, ["Hacker News"]);
        assert_eq!(news.signals.tranco_rank, Some(900));
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn shared_front_pages_keep_only_shared_facts() {
        let claim = |item: &str, url: &str, country: Option<&str>, kinds: &[&str]| {
            let mut site = site(item, item, url);
            site.country = country.map(str::to_string);
            site.kinds = kinds.iter().map(|k| k.to_string()).collect();
            site
        };
        let mut builder = Builder::new();
        builder.add_official_sites(&[
            claim("Q1", "x.com", Some("US"), &["social media company"]),
            claim("Q2", "x.com", Some("US"), &["bank", "online bank"]),
            claim(
                "Q3",
                "continental.com",
                Some("DE"),
                &["tire manufacturer", "airline"],
            ),
            claim("Q4", "continental.com", Some("US"), &["airlines"]),
            claim("Q5", "usbank.com", Some("US"), &["bank"]),
        ]);
        let records = builder.finish(None);
        let get = |domain: &str| records.iter().find(|r| r.domain == domain).unwrap();
        assert_eq!(get("x.com").country.as_deref(), Some("US"));
        assert!(get("x.com").kinds.is_empty());
        assert_eq!(get("continental.com").country, None);
        assert_eq!(get("continental.com").kinds, ["airline"]);
        assert_eq!(get("usbank.com").kinds, ["bank"]);
    }

    #[test]
    fn a_far_better_known_item_gives_a_shared_front_page_its_facts() {
        let claim = |item: &str, kinds: &[&str], about: &str, sitelinks: u32| {
            let mut site = site(item, item, "wikipedia.org");
            site.kinds = kinds.iter().map(|k| k.to_string()).collect();
            site.about = Some(about.to_string());
            site.sitelinks = sitelinks;
            site
        };
        let mut builder = Builder::new();
        builder.add_official_sites(&[
            claim("Q4", &["online community"], "editors of Wikipedia", 40),
            claim(
                "Q52",
                &["online encyclopedia", "wiki"],
                "free online encyclopedia",
                330,
            ),
        ]);
        let records = builder.finish(None);
        assert_eq!(records[0].kinds, ["online encyclopedia", "wiki"]);
        assert_eq!(
            records[0].about.as_deref(),
            Some("free online encyclopedia")
        );

        // Two items about as well known still keep only what they share.
        let mut builder = Builder::new();
        builder.add_official_sites(&[
            claim("Q4", &["online community"], "editors of Wikipedia", 200),
            claim(
                "Q52",
                &["online encyclopedia"],
                "free online encyclopedia",
                330,
            ),
        ]);
        let records = builder.finish(None);
        assert!(records[0].kinds.is_empty());
        assert_eq!(records[0].about, None);
    }

    #[test]
    fn other_names_follow_the_main_names() {
        let mut nyt = site("Q9684", "The New York Times", "nytimes.com");
        nyt.names = vec!["NYT".into(), "New York Times".into()];
        let mut builder = Builder::new();
        builder.add_official_sites(&[nyt, site("Q2", "NYT Company", "nytimes.com")]);
        let records = builder.finish(None);
        assert_eq!(
            records[0].aliases,
            ["The New York Times", "NYT Company", "NYT", "New York Times"]
        );
    }

    #[test]
    fn an_inner_page_naming_its_site_counts_when_no_front_page_does() {
        let got = official(&[
            ("Q1", "Perplexity AI", "https://www.perplexity.ai/hub/"),
            ("Q2", "Rocky", "https://bit.ly/RockyHeavyweightCollection"),
            ("Q3", "Sergey Karjakin", "https://t.me/karjakin"),
            // A hotel of the chain is not the chain.
            ("Q7", "Hilton Athens", "https://www.hilton.ru/athens"),
            // A front page claim wins over an inner page naming the site.
            ("Q4", "Honda", "https://www.honda.com/"),
            ("Q5", "Honda Super Cub", "https://www.honda.com/supercub"),
            // A subdomain is still a part of the site, not the site.
            ("Q6", "City of Milwaukee", "https://city.milwaukee.gov/"),
        ]);
        assert_eq!(
            got,
            [
                official_with("honda.com", &["Honda"]),
                official_with("perplexity.ai", &["Perplexity AI"]),
            ]
        );
    }

    #[test]
    fn official_sites_skip_shared_hosts() {
        let mut sites: Vec<OfficialSite> = (1..=5)
            .map(|i| site(&format!("Q{i}"), &format!("Brand {i}"), "five.com"))
            .collect();
        sites.extend((1..=6).map(|i| site(&format!("Q{i}"), "Brand", "six.com")));
        // The same item listed twice counts once.
        sites.push(site("Q1", "Brand 1", "five.com"));
        sites.push(site("Q9", "Q9", "plain.com"));
        let mut builder = Builder::new();
        builder.add_official_sites(&sites);
        assert_eq!(builder.len(), 2);
        let records = builder.finish(None);
        let five = records.iter().find(|r| r.domain == "five.com").unwrap();
        assert!(five.signals.official_site);
        assert_eq!(
            five.aliases,
            ["Brand 1", "Brand 2", "Brand 3", "Brand 4", "Brand 5"]
        );
        let plain = records.iter().find(|r| r.domain == "plain.com").unwrap();
        assert!(plain.signals.official_site);
        assert!(plain.aliases.is_empty());
        assert!(records.iter().all(|r| r.domain != "six.com"));

        // Every spelling of a front page is the same front page.
        let got = official(&[
            ("Q1", "A", "http://six.com"),
            ("Q2", "B", "https://www.six.com/"),
            ("Q3", "C", "https://six.com/en/"),
            ("Q4", "D", "https://six.com/?from=wikidata"),
            ("Q5", "E", "https://six.com/index.html"),
            ("Q6", "F", "six.com"),
        ]);
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn colleges_on_subdomains_leave_the_university_official() {
        let colleges = [
            ("Q101", "Balliol College", "https://www.balliol.ox.ac.uk/"),
            ("Q102", "Merton College", "https://www.merton.ox.ac.uk/"),
            ("Q103", "Magdalen College", "https://www.magd.ox.ac.uk/"),
            ("Q104", "Christ Church", "https://www.chch.ox.ac.uk/"),
            ("Q105", "New College", "https://www.new.ox.ac.uk/"),
            ("Q106", "Exeter College", "https://www.exeter.ox.ac.uk/"),
        ];
        let mut claims = vec![("Q100", "University of Oxford", "https://www.ox.ac.uk/")];
        claims.extend(colleges);
        assert_eq!(
            official(&claims),
            [official_with("ox.ac.uk", &["University of Oxford"])]
        );
        // Nor do the colleges make ox.ac.uk anyone's site on their own.
        assert!(official(&colleges).is_empty());
    }

    #[test]
    fn eu_bodies_leave_the_eu_official() {
        let bodies = [
            ("Q201", "EU Commission", "https://commission.europa.eu/"),
            ("Q202", "EU Parliament", "https://www.europarl.europa.eu/"),
            ("Q203", "EU Council", "https://www.consilium.europa.eu/"),
            ("Q204", "EU Central Bank", "https://www.ecb.europa.eu/"),
            ("Q205", "EU Court of Justice", "https://curia.europa.eu/"),
            // Inner pages of the EU's own host count no more than subdomains.
            ("Q206", "EU Regions Committee", "https://europa.eu/cor_en"),
            ("Q207", "EU Ombudsman", "https://www.europa.eu/ombudsman"),
        ];
        let mut claims = vec![("Q200", "European Union", "https://europa.eu/")];
        claims.extend(bodies);
        assert_eq!(
            official(&claims),
            [official_with("europa.eu", &["European Union"])]
        );
        assert!(official(&bodies).is_empty());
    }

    #[test]
    fn language_editions_leave_wikipedia_official() {
        let mut claims = vec![("Q300", "Wikipedia", "https://www.wikipedia.org/")];
        claims.extend([
            ("Q301", "English Wikipedia", "https://en.wikipedia.org/"),
            ("Q302", "German Wikipedia", "https://de.wikipedia.org/"),
            ("Q303", "French Wikipedia", "https://fr.wikipedia.org/"),
            ("Q304", "Japanese Wikipedia", "https://ja.wikipedia.org/"),
            ("Q305", "Spanish Wikipedia", "https://es.wikipedia.org/"),
            ("Q306", "Russian Wikipedia", "https://ru.wikipedia.org/"),
            ("Q307", "Italian Wikipedia", "https://it.wikipedia.org/"),
        ]);
        assert_eq!(
            official(&claims),
            [official_with("wikipedia.org", &["Wikipedia"])]
        );
    }

    #[test]
    fn profile_pages_do_not_make_the_host_official() {
        let tenants = [
            ("Q401", "Acme Rockets", "https://linktr.ee/acmerockets"),
            ("Q402", "Globex", "https://linktr.ee/globex"),
            ("Q403", "Initech", "https://linktr.ee/initech"),
        ];
        assert!(official(&tenants).is_empty());
        // Nor do they stop linktr.ee from being Linktree's own site.
        let mut claims = vec![("Q400", "Linktree", "https://linktr.ee/")];
        claims.extend(tenants);
        assert_eq!(
            official(&claims),
            [official_with("linktr.ee", &["Linktree"])]
        );
    }

    #[test]
    fn malformed_seed_domains_are_dropped() {
        let mut builder = Builder::new();
        builder.add_tranco(&[
            TrancoEntry {
                rank: 1,
                domain: "a..b.com".into(),
            },
            TrancoEntry {
                rank: 2,
                domain: "WWW.Good.com".into(),
            },
        ]);
        builder.add_cc_ranks(&[
            CcRank {
                domain: "x..com".into(),
                harmonic_rank: 1,
                pagerank_rank: Some(1),
                n_hosts: None,
            },
            CcRank {
                domain: format!("{}.com", "a".repeat(70_000)),
                harmonic_rank: 2,
                pagerank_rank: None,
                n_hosts: None,
            },
        ]);
        // Hand-built claims that OfficialSite::new would have refused.
        let junk = |url: &str, host: &str| OfficialSite {
            item: "Q1".into(),
            label: "Junk".into(),
            url: url.into(),
            host: host.into(),
            path: "/".into(),
            domain: host.into(),
            country: None,
            kinds: Vec::new(),
            names: Vec::new(),
            about: None,
            sitelinks: 0,
            intro: None,
        };
        builder.add_official_sites(&[
            junk("mailto:a@b.com", "a@b.com"),
            junk("https://com..x/", "com..x"),
        ]);
        let mut extract = WatExtract::new();
        extract
            .linking_domains
            .entry(format!("{}.com", "h".repeat(70_000)))
            .or_default()
            .insert("good.com".into());
        builder.add_wat(&extract);

        let records = builder.finish(None);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].domain, "good.com");
        assert_eq!(records[0].signals.tranco_rank, Some(2));
        assert!(!records[0].signals.official_site);
    }

    #[test]
    fn wat_fills_gaps_and_fresh_crawls_win() {
        let mut extract = WatExtract::new();
        extract.homepages.insert(
            "x.com".into(),
            crate::HomepageMeta {
                domain: "x.com".into(),
                url: "https://x.com/".into(),
                title: Some("From WAT".into()),
                description: Some("WAT description".into()),
                site_name: None,
            },
        );
        let mut crawled = SiteRecord::new("x.com");
        crawled.title = Some("From crawl".into());
        crawled.crawled_at = Some(1_700_000_000);

        for wat_first in [true, false] {
            let mut builder = Builder::new();
            if wat_first {
                builder.add_wat(&extract);
                builder.add_records([crawled.clone()]);
            } else {
                builder.add_records([crawled.clone()]);
                builder.add_wat(&extract);
            }
            let record = builder.finish(None).remove(0);
            assert_eq!(record.title.as_deref(), Some("From crawl"), "{wat_first}");
            assert_eq!(record.description.as_deref(), Some("WAT description"));
            assert_eq!(record.url.as_deref(), Some("https://x.com/"));
        }
    }

    #[test]
    fn wat_link_texts_keep_the_most_frequent() {
        let mut extract = WatExtract::new();
        let texts = extract.anchors.entry("x.com".into()).or_default();
        // Text i is used by more sites the higher i is; text 99 by so many
        // that every bit is set.
        for i in 0..100u32 {
            texts.insert(format!("text {i}"), u64::MAX >> (63 - i * 63 / 99));
        }
        extract
            .linking_domains
            .entry("x.com".into())
            .or_default()
            .extend(["a.com".to_string(), "b.com".to_string()]);
        let mut builder = Builder::new();
        builder.add_wat(&extract);
        builder.add_wat(&extract);
        let record = builder.finish(None).remove(0);
        assert_eq!(record.link_texts.len(), MAX_LINK_TEXTS);
        assert_eq!(record.link_texts[0].text, "text 99");
        // Adding the same WAT data twice counts its sites once.
        assert_eq!(record.link_texts[0].count, MAX_LINKER_ESTIMATE);
        assert_eq!(record.signals.linking_domains, 2);
    }

    #[test]
    fn finish_cuts_to_top_n() {
        let mut builder = Builder::new();
        builder.add_tranco(&[
            TrancoEntry {
                rank: 2,
                domain: "b.com".into(),
            },
            TrancoEntry {
                rank: 1,
                domain: "a.com".into(),
            },
            TrancoEntry {
                rank: 3,
                domain: "c.com".into(),
            },
        ]);
        let top: Vec<String> = builder
            .finish(Some(2))
            .into_iter()
            .map(|r| r.domain)
            .collect();
        assert_eq!(top, ["a.com", "b.com"]);
    }

    #[test]
    fn finish_keeps_the_same_order_as_a_full_sort() {
        // Many ties: equal ranks, official or not, some with link texts.
        let builder = || {
            let mut builder = Builder::new();
            let entries: Vec<TrancoEntry> = (0..60u32)
                .map(|i| TrancoEntry {
                    rank: 1 + (i * 7) % 13,
                    domain: format!("site{i}.com"),
                })
                .collect();
            builder.add_tranco(&entries);
            let official: Vec<OfficialSite> = (0..60)
                .step_by(4)
                .map(|i| {
                    site(
                        &format!("Q{i}"),
                        &format!("Site {i}"),
                        &format!("site{i}.com"),
                    )
                })
                .collect();
            builder.add_official_sites(&official);
            let mut extract = WatExtract::new();
            for i in (0..60).step_by(3) {
                extract
                    .anchors
                    .entry(format!("site{i}.com"))
                    .or_default()
                    .insert(format!("site {i}"), linker_bit("a.com"));
            }
            builder.add_wat(&extract);
            builder
        };
        let full = builder().finish(None);
        assert_eq!(full.len(), 60);
        for n in [0, 1, 5, 13, 30, 59, 60, 1000] {
            assert_eq!(builder().finish(Some(n)), full[..n.min(60)], "top {n}");
        }
    }
}
