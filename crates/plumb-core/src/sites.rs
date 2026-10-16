//! A node's sites as their stored items ([`crate::site`]), by domain:
//! [`Sites`] does for items what [`RecordSet`] does for records, by the
//! same rules. Adding a site that is there already merges the two, and
//! seed data read again takes back what a subdomain site or a misread
//! Wikidata claim gave a site. Code that keeps sites moves off
//! [`RecordSet`] onto it (step 4 of the data format plan), and each of its
//! operations is tested against the record set's: doing it to the records'
//! stored items gives the stored item of each record the record set holds.

use std::cmp::Reverse;
use std::collections::HashMap;

use plumb_format::Item;

use crate::site_item::tidy;
use crate::{canonical_domain, parent_domain, site};
#[cfg(doc)]
use crate::{RecordSet, SITES_VERSION};

/// Sites' stored items keyed by domain; adding a site for a domain already
/// present merges them.
///
/// Each item is boxed: an [`Item`] is several hundred bytes even when
/// nearly empty, and a hash table keeps room for about twice its entries,
/// so a million sites held inline would take a gigabyte of table alone,
/// and twice that while it grows.
#[derive(Debug, Clone, Default)]
pub struct Sites {
    map: HashMap<String, Box<Item>>,
}

impl Sites {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&self, domain: &str) -> Option<&Item> {
        self.map.get(domain).map(|site| &**site)
    }

    /// The site of `domain`, created with nothing but its domain if it is
    /// not there yet. `domain` must already be canonical, as
    /// [`crate::registrable_domain`] returns it.
    pub fn entry(&mut self, domain: &str) -> &mut Item {
        self.map
            .entry(domain.to_string())
            .or_insert_with(|| Box::new(site::new(domain)))
    }

    /// Takes off parent domains what they got from `seed` items of their
    /// subdomain sites ([`parent_domain`]) when those were folded into them:
    /// names, Wikidata's description ([`site::wikidata_about`]) and the
    /// Wikipedia intro ([`site::intro`]). For seed data read again after
    /// the subdomain lists grew ([`SITES_VERSION`]), before it is upserted;
    /// the parent's own seed item adds its own back. The seed's text is
    /// compared cleaned, as the set holds it ([`site::clean`]).
    pub fn split_subdomain_sites(&mut self, seed: &[Item]) {
        // Whether the parent's text is there and is the seed's, cleaned.
        let same = |parent: Option<&str>, seed: Option<&str>| {
            parent.is_some() && parent == seed.map(str::to_string).and_then(tidy).as_deref()
        };
        for subsite in seed {
            let Some(parent) = parent_domain(site::domain(subsite)) else {
                continue;
            };
            let Some(site) = self.map.get_mut(parent) else {
                continue;
            };
            let names: Vec<String> = subsite.names.iter().cloned().filter_map(tidy).collect();
            site.names.retain(|name| !names.contains(name));
            if same(site::wikidata_about(site), site::wikidata_about(subsite)) {
                site::set_wikidata_about(site, None);
            }
            if same(site::intro(site), site::intro(subsite)) {
                site::set_intro(site, None);
            }
        }
    }

    /// Takes back what a misread Wikidata claim (an email address read as a
    /// website) gave the site of `domain`: the item's `names`, and the facts
    /// official websites bring (whether it is one, its sitelinks, country,
    /// kinds, Wikidata's description and the intro), which the domain's
    /// own seed item adds back when it is upserted after. `names` are
    /// compared cleaned, as the set holds them ([`site::clean`]).
    pub fn take_back_official_site(&mut self, domain: &str, names: &[&str]) {
        let Some(site) = self.map.get_mut(domain) else {
            return;
        };
        let names: Vec<String> = names
            .iter()
            .filter_map(|name| tidy(name.to_string()))
            .collect();
        site.names.retain(|name| !names.contains(name));
        let mut signals = site::signals(site);
        signals.official_site = false;
        signals.sitelinks = 0;
        site::set_signals(site, &signals);
        site::set_country(site, None);
        site::set_kinds(site, Vec::new());
        site::set_wikidata_about(site, None);
        site::set_intro(site, None);
    }

    /// Inserts a site, merging it into the one already there for its
    /// domain ([`site::merge`]). The domain, the item's id, is made
    /// canonical first ([`canonical_domain`]), so `Example.COM` and
    /// `münchen.de` land on `example.com` and `xn--mnchen-3ya.de`, the item
    /// is cleaned ([`site::clean`]), and page fields read off a bot check
    /// are dropped ([`site::drop_bot_check`]). Returns false, dropping the
    /// item, when its id is not a valid registrable domain.
    pub fn upsert(&mut self, site: Item) -> bool {
        self.upsert_with(site, site::merge)
    }

    /// [`Sites::upsert`] for a crawl another node shared, merged with
    /// [`site::merge_shared`].
    pub fn upsert_shared(&mut self, site: Item) -> bool {
        self.upsert_with(site, site::merge_shared)
    }

    fn upsert_with(&mut self, mut site: Item, merge: impl FnOnce(&mut Item, Item)) -> bool {
        match canonical_domain(&site.id) {
            Some(domain) => site.id = domain,
            None => return false,
        }
        site::clean(&mut site);
        // A bot check crawled in place of the homepage, by this node or by
        // one whose crawl it took, before crawlers knew to skip them.
        site::drop_bot_check(&mut site);
        match self.map.get_mut(&site.id) {
            Some(existing) => merge(existing, site),
            None => {
                self.map.insert(site.id.clone(), Box::new(site));
            }
        }
        true
    }

    pub fn iter(&self) -> impl Iterator<Item = &Item> {
        self.map.values().map(|site| &**site)
    }

    /// Keeps only the sites for which `keep` is true.
    pub fn retain(&mut self, mut keep: impl FnMut(&Item) -> bool) {
        self.map.retain(|_, site| keep(site));
    }

    /// All sites, best [`site::link_score`] first, ties broken by domain.
    pub fn into_sorted_vec(self) -> Vec<Item> {
        let mut sites: Vec<Item> = self.into_iter().collect();
        sort_by_link_score(&mut sites);
        sites
    }
}

/// All sites, in no particular order. Each item is freed as soon as it is
/// taken, so a caller that keeps only a little of each never holds two
/// copies of the set.
impl IntoIterator for Sites {
    type Item = Item;
    type IntoIter = std::iter::Map<
        std::collections::hash_map::IntoValues<String, Box<Item>>,
        fn(Box<Item>) -> Item,
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.map.into_values().map(|site| *site)
    }
}

impl Extend<Item> for Sites {
    fn extend<I: IntoIterator<Item = Item>>(&mut self, iter: I) {
        for site in iter {
            self.upsert(site);
        }
    }
}

impl FromIterator<Item> for Sites {
    fn from_iter<I: IntoIterator<Item = Item>>(iter: I) -> Self {
        let mut sites = Sites::new();
        sites.extend(iter);
        sites
    }
}

/// Sorts best [`site::link_score`] first, ties broken by domain so output
/// is stable, as [`crate::sort_by_link_score`] sorts records. Each score is
/// read off its item's fields once, not at every comparison.
pub fn sort_by_link_score(sites: &mut [Item]) {
    sites.sort_by_cached_key(|site| {
        (
            Reverse(total_order(site::link_score(site))),
            site.id.clone(),
        )
    });
}

/// A number that orders as `score` does by [`f32::total_cmp`], by which
/// records are sorted, so that a sort can keep it as its key.
fn total_order(score: f32) -> i32 {
    let bits = score.to_bits() as i32;
    bits ^ (((bits >> 31) as u32) >> 1) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::site::tests::variants;
    use crate::site_item::tests::full_record;
    use crate::{stored, RecordSet, SiteRecord};

    /// Checks that `sites` holds, for every site, the stored item of the
    /// clean record `records` holds, both as an item and as a stored line.
    fn assert_same(sites: &Sites, records: &RecordSet, what: &str) {
        assert_eq!(sites.len(), records.len(), "{what}");
        assert_eq!(sites.is_empty(), records.is_empty(), "{what}");
        for record in records.iter() {
            let domain = &record.domain;
            let site = sites.get(domain);
            let site = site.unwrap_or_else(|| panic!("{what}: {domain} is missing"));
            assert_eq!(site, &record.to_stored_item(), "{what}: {domain}");
            let line = serde_json::to_vec(site).unwrap();
            assert_eq!(line, stored::line(record).unwrap(), "{what}: {domain}");
        }
    }

    /// Records as a node takes them in, not all clean: python.org seven
    /// ways (the variants merging is tested on) and once more with text
    /// and a URL to clean; domains not written in their canonical form; a
    /// bot check read in place of a homepage, then the homepage; and a
    /// domain that is none.
    fn incoming() -> Vec<SiteRecord> {
        let mut records = variants();
        let mut dirty = full_record();
        dirty.url = Some("HTTPS://WWW.Python.org/index%2ehtml?".into());
        dirty.title = Some("  Welcome\u{7} to Python ".into());
        dirty.aliases.push(" Python".into());
        dirty.aliases.push("Dư\u{323} ba\u{301}o".into());
        records.push(dirty);
        let mut example = SiteRecord::new("Example.COM");
        example.signals.tranco_rank = Some(10);
        records.push(example);
        let mut example = SiteRecord::new("example.com");
        example.add_alias("Example");
        records.push(example);
        let mut munich = SiteRecord::new("münchen.de");
        munich.title = Some("München".into());
        records.push(munich);
        let mut munich = SiteRecord::new("xn--mnchen-3ya.de");
        munich.aliases = vec!["Munich".into()];
        munich.signals.harmonic_rank = Some(5_000);
        records.push(munich);
        let mut check = SiteRecord::new("usbank.com");
        check.url = Some("https://www.google.com/sorry/index".into());
        check.title = Some("Sorry...".into());
        check.headings = vec!["About this page".into()];
        check.crawled_at = Some(1_791_605_520);
        records.push(check);
        let mut bank = SiteRecord::new("usbank.com");
        bank.url = Some("https://www.usbank.com/".into());
        bank.title = Some("U.S. Bank".into());
        bank.crawled_at = Some(1_791_700_000);
        records.push(bank);
        records.push(SiteRecord::new("not a domain"));
        records
    }

    /// Crawls other nodes shared, without page text or headings: of
    /// python.org, of rust-lang.org, which this node crawled itself before,
    /// and of a site not yet in the set.
    fn shared() -> Vec<SiteRecord> {
        let mut python = variants()[5].clone();
        assert_eq!(python.body_text, None);
        python.crawled_at = Some(1_791_950_000);
        let mut rust = variants()[5].clone();
        rust.domain = "rust-lang.org".into();
        rust.url = Some("https://www.rust-lang.org/".into());
        rust.title = Some("Rust Programming Language".into());
        let mut crates = rust.clone();
        crates.domain = "crates.io".into();
        crates.url = Some("https://crates.io/".into());
        crates.title = Some("crates.io: Rust Package Registry".into());
        vec![python, rust, crates]
    }

    /// The record set and the set of sites the same records make, the
    /// records upserted one at a time and the sites their stored items.
    fn both(records: &[SiteRecord]) -> (RecordSet, Sites) {
        let mut record_set = RecordSet::new();
        let mut sites = Sites::new();
        for record in records {
            let site = record.clone().stored_item_as_is();
            let upserted = record_set.upsert(record.clone());
            assert_eq!(sites.upsert(site), upserted, "{}", record.domain);
            assert_same(&sites, &record_set, &format!("upsert {}", record.domain));
        }
        (record_set, sites)
    }

    #[test]
    fn sites_upsert_as_records_do() {
        let (mut records, mut sites) = both(&incoming());
        assert_eq!(sites.len(), 4);
        assert!(sites.get("example.com").is_some());
        assert!(sites.get("xn--mnchen-3ya.de").is_some());
        assert!(sites.get("not a domain").is_none());
        assert!(!sites.upsert(SiteRecord::new("co.uk").stored_item_as_is()));
        let bank = sites.get("usbank.com").unwrap();
        assert_eq!(bank.title.as_deref(), Some("U.S. Bank"));
        // Of the bot check alone, nothing but the domain is left.
        let check = incoming()
            .into_iter()
            .find(|record| record.title.as_deref() == Some("Sorry..."));
        let mut checked = Sites::new();
        assert!(checked.upsert(check.unwrap().stored_item_as_is()));
        assert_eq!(checked.get("usbank.com"), Some(&site::new("usbank.com")));

        let mut rust = SiteRecord::new("rust-lang.org");
        rust.body_text = Some("A language empowering everyone.".into());
        rust.headings = vec!["Why Rust?".into()];
        rust.crawled_at = Some(1_791_600_000);
        assert_eq!(
            sites.upsert(rust.clone().stored_item_as_is()),
            records.upsert(rust)
        );
        for record in shared() {
            let site = record.clone().stored_item_as_is();
            let upserted = records.upsert_shared(record.clone());
            assert!(upserted);
            assert_eq!(sites.upsert_shared(site), upserted);
            assert_same(
                &sites,
                &records,
                &format!("upsert_shared {}", record.domain),
            );
        }
        // What the fresher shared crawl left empty stays.
        let rust = sites.get("rust-lang.org").unwrap();
        assert_eq!(rust.title.as_deref(), Some("Rust Programming Language"));
        assert_eq!(
            rust.text.as_deref(),
            Some("A language empowering everyone.")
        );
        assert_eq!(site::headings(rust), ["Why Rust?"]);
        assert_eq!(sites.len(), 6);
    }

    #[test]
    fn an_entry_is_the_site_or_a_new_one_as_with_records() {
        let (mut records, mut sites) = both(&incoming());
        for domain in ["python.org", "plumbsearch.org"] {
            records.entry(domain).add_alias("Plumb Search");
            site::add_alias(sites.entry(domain), "Plumb Search");
        }
        records.entry("rust-lang.org");
        assert_eq!(sites.entry("rust-lang.org"), &site::new("rust-lang.org"));
        assert_same(&sites, &records, "entry");
        assert_eq!(sites.len(), 6);
    }

    #[test]
    fn subdomain_sites_take_their_names_off_their_parent_as_with_records() {
        let mut parent = SiteRecord::new("ycombinator.com");
        parent.aliases = vec!["Y Combinator".into(), "Hacker News".into()];
        parent.about = Some("social news website".into());
        parent.intro = Some("Hacker News is a social news website.".into());
        let mut google = SiteRecord::new("google.com");
        google.aliases = vec!["Google".into()];
        google.about = Some("search engine".into());
        let (mut records, mut sites) = both(&[parent, google, full_record()]);

        // Seed text not yet cleaned, as the set holds it.
        let mut news = SiteRecord::new("news.ycombinator.com");
        news.aliases = vec!["Hacker News ".into()];
        news.about = Some("social news\twebsite".into());
        news.intro = Some("Hacker News is a social\u{7}news website.".into());
        let mut yc = SiteRecord::new("ycombinator.com");
        yc.aliases = vec!["Y Combinator".into()];
        yc.about = Some("startup accelerator".into());
        // A subdomain site whose parent has nothing of it.
        let mut scholar = SiteRecord::new("scholar.google.com");
        scholar.aliases = vec!["Google Scholar".into()];
        scholar.about = Some("academic search engine".into());
        let seed = vec![news, yc, scholar];
        let seed_items: Vec<Item> = seed
            .iter()
            .map(|record| record.clone().stored_item_as_is())
            .collect();

        records.split_subdomain_sites(&seed);
        sites.split_subdomain_sites(&seed_items);
        assert_same(&sites, &records, "split");
        let yc = sites.get("ycombinator.com").unwrap();
        assert_eq!(yc.names, ["Y Combinator"]);
        assert_eq!(site::wikidata_about(yc), None);
        assert_eq!(site::intro(yc), None);
        let google = sites.get("google.com").unwrap();
        assert_eq!(site::wikidata_about(google), Some("search engine"));

        records.extend(seed);
        sites.extend(seed_items);
        assert_same(&sites, &records, "split, then the seed");
        let yc = sites.get("ycombinator.com").unwrap();
        assert_eq!(site::wikidata_about(yc), Some("startup accelerator"));
        assert_eq!(
            sites.get("news.ycombinator.com").unwrap().names,
            ["Hacker News"]
        );
    }

    #[test]
    fn an_official_site_is_taken_back_as_from_records() {
        let (mut records, mut sites) = both(&incoming());
        let names = ["Python Software Foundation ", "Nothing"];
        for domain in ["python.org", "example.com", "plumbsearch.org"] {
            records.take_back_official_site(domain, &names);
            sites.take_back_official_site(domain, &names);
        }
        assert_same(&sites, &records, "take_back_official_site");
        let python = sites.get("python.org").unwrap();
        assert!(python.names.contains(&"Python".to_string()));
        assert!(!python
            .names
            .contains(&"Python Software Foundation".to_string()));
        let signals = site::signals(python);
        assert!(!signals.official_site);
        assert_eq!(signals.sitelinks, 0);
        assert_eq!(signals.tranco_rank, Some(400));
        assert!(site::kinds(python).is_empty());
        assert_eq!(site::country(python), None);
        assert_eq!(site::wikidata_about(python), None);
        assert_eq!(site::intro(python), None);
        assert!(site::summary(python).is_some());
    }

    #[test]
    fn sites_are_kept_and_sorted_as_records_are() {
        let (mut records, mut sites) = both(&incoming());
        // Sites without a score, which sort by domain.
        for domain in ["rust-lang.org", "plumbsearch.org"] {
            records.upsert(SiteRecord::new(domain));
            sites.upsert(site::new(domain));
        }
        let (all_records, all_sites) = (records.clone(), sites.clone());

        records.retain(|record| record.domain != "example.com");
        sites.retain(|site| site.id != "example.com");
        assert_same(&sites, &records, "retain by domain");
        records.retain(|record| record.title.is_none() || record.link_score() > 0.5);
        sites.retain(|site| site.title.is_none() || site::link_score(site) > 0.5);
        assert_same(&sites, &records, "retain by title and score");
        assert_eq!(sites.len(), 3);

        let records = all_records.into_sorted_vec();
        let sites = all_sites.into_sorted_vec();
        let domains: Vec<&str> = sites.iter().map(|site| site.id.as_str()).collect();
        let record_domains: Vec<&str> = records.iter().map(|r| r.domain.as_str()).collect();
        assert_eq!(domains, record_domains);
        assert_eq!(
            domains,
            [
                "python.org",
                "example.com",
                "xn--mnchen-3ya.de",
                "plumbsearch.org",
                "rust-lang.org",
                "usbank.com"
            ]
        );
        for (site, record) in sites.iter().zip(&records) {
            assert_eq!(site, &record.to_stored_item());
        }
        let mut sorted: Vec<Item> = records
            .iter()
            .rev()
            .map(SiteRecord::to_stored_item)
            .collect();
        sort_by_link_score(&mut sorted);
        assert_eq!(sorted, sites);
    }

    #[test]
    fn sites_collect_and_extend_as_records_do() {
        let all = incoming();
        let (first, rest) = all.split_at(5);
        let stored = |records: &[SiteRecord]| -> Vec<Item> {
            records
                .iter()
                .map(|record| record.clone().stored_item_as_is())
                .collect()
        };
        let mut records: RecordSet = first.iter().cloned().collect();
        let mut sites: Sites = stored(first).into_iter().collect();
        assert_same(&sites, &records, "collect");
        records.extend(rest.iter().cloned());
        sites.extend(stored(rest));
        assert_same(&sites, &records, "extend");

        let mut taken: Vec<Item> = sites.into_iter().collect();
        taken.sort_by(|a, b| a.id.cmp(&b.id));
        let mut expected: Vec<Item> = records
            .into_iter()
            .map(|record| record.to_stored_item())
            .collect();
        expected.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(taken, expected);
    }

    #[test]
    fn scores_order_as_they_compare() {
        let scores = [
            f32::NEG_INFINITY,
            -1.0,
            -0.0,
            0.0,
            f32::MIN_POSITIVE,
            0.25,
            1.0,
            f32::INFINITY,
            f32::NAN,
        ];
        for a in scores {
            for b in scores {
                assert_eq!(
                    total_order(a).cmp(&total_order(b)),
                    a.total_cmp(&b),
                    "{a} {b}"
                );
            }
        }
    }
}
