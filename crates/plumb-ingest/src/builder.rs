//! Folds every seed source into one set of [`SiteRecord`]s.

use std::collections::{HashMap, HashSet};

use plumb_core::{linker_count, RecordSet, SiteRecord, MAX_LINK_TEXTS};
use tracing::info;

use crate::{CcRank, OfficialSite, TrancoEntry, WatExtract};

/// More distinct Wikidata items than this claiming one domain marks it as a
/// shared host rather than anyone's official site.
const MAX_ITEMS_PER_OFFICIAL_DOMAIN: usize = 5;

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
    /// Entries with an empty domain or rank 0 are ignored.
    pub fn add_tranco(&mut self, entries: &[TrancoEntry]) {
        for entry in entries {
            if entry.domain.is_empty() || entry.rank == 0 {
                continue;
            }
            let signals = &mut self.records.entry(&entry.domain).signals;
            signals.tranco_rank = best_rank(signals.tranco_rank, Some(entry.rank));
        }
    }

    /// Sets `signals.harmonic_rank` and `signals.pagerank_rank`.
    ///
    /// A domain that already has ranks keeps the better (lower) ones.
    /// Entries with an empty domain or harmonic rank 0 are ignored.
    pub fn add_cc_ranks(&mut self, ranks: &[CcRank]) {
        for rank in ranks {
            if rank.domain.is_empty() || rank.harmonic_rank == 0 {
                continue;
            }
            let signals = &mut self.records.entry(&rank.domain).signals;
            signals.harmonic_rank = best_rank(signals.harmonic_rank, Some(rank.harmonic_rank));
            signals.pagerank_rank =
                best_rank(signals.pagerank_rank, rank.pagerank_rank.filter(|&r| r > 0));
        }
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

    /// Sets `signals.official_site` and adds the Wikidata label as an alias.
    /// A domain claimed by more than five different items is a shared host
    /// (a social network, a site builder), so it is skipped entirely.
    ///
    /// A label that is just the item id (what Wikidata's label service
    /// returns for items without an English label) is not added as an alias.
    pub fn add_official_sites(&mut self, sites: &[OfficialSite]) {
        let mut items_per_domain: HashMap<&str, HashSet<&str>> = HashMap::new();
        for site in sites {
            items_per_domain
                .entry(site.domain.as_str())
                .or_default()
                .insert(site.item.as_str());
        }
        let is_shared = |domain: &str| {
            items_per_domain
                .get(domain)
                .is_some_and(|items| items.len() > MAX_ITEMS_PER_OFFICIAL_DOMAIN)
        };
        for site in sites {
            if site.domain.is_empty() || is_shared(&site.domain) {
                continue;
            }
            let record = self.records.entry(&site.domain);
            record.signals.official_site = true;
            let label = site.label.trim();
            if label != site.item {
                record.add_alias(label);
            }
        }
        let shared = items_per_domain
            .iter()
            .filter(|(_, items)| items.len() > MAX_ITEMS_PER_OFFICIAL_DOMAIN)
            .count();
        if shared > 0 {
            info!(
                "skipped {shared} domains claimed as official site by more than {MAX_ITEMS_PER_OFFICIAL_DOMAIN} Wikidata items"
            );
        }
    }

    /// Merges ready-made records, e.g. from a previous run or a crawl.
    pub fn add_records<I: IntoIterator<Item = SiteRecord>>(&mut self, records: I) {
        self.records.extend(records);
    }

    /// All records, best link score first, cut to `top_n` when given.
    pub fn finish(self, top_n: Option<usize>) -> Vec<SiteRecord> {
        let mut records = self.records.into_sorted_vec();
        if let Some(n) = top_n {
            records.truncate(n);
        }
        records
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

    fn site(item: &str, label: &str, domain: &str) -> OfficialSite {
        OfficialSite {
            item: item.into(),
            label: label.into(),
            url: format!("https://{domain}/"),
            domain: domain.into(),
        }
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
            }
        );
        assert_eq!(usbank.crawled_at, None);

        // Six items claim facebook.com, so it is a shared host, not an official site.
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
}
