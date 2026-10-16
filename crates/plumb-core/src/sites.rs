//! A node's sites as their stored items ([`crate::site`]), by domain:
//! [`Sites`] does for items what [`RecordSet`] does for records, by the
//! same rules. Adding a site that is there already merges the two, and
//! seed data read again takes back what a subdomain site or a misread
//! Wikidata claim gave a site. Code that keeps sites moves off
//! [`RecordSet`] onto it (step 4 of the data format plan), and each of its
//! operations is tested against the record set's: doing it to the records'
//! stored items gives the stored item of each record the record set holds.
//!
//! A set holds each site as its stored line ([`stored::write_item`]), not
//! as its item. An item is a wide struct, and each JSON object in it (its
//! fields, and every object among them: its ranks, crawl marks, each link
//! text) is a B-tree node of over 600 bytes however little it holds. So a
//! site known only by its ranks takes about 2.1 kB as an item, 0.7 kB as a
//! record and 0.15 kB as its line, and a crawled site about 12 kB, 3 kB
//! and a little over its line's 1.6 kB. A site is read from its line when
//! it is asked for ([`Sites::get`], [`Sites::iter`]), and written again
//! when it changes ([`Sites::update`], [`Sites::upsert`]). Its link score
//! is kept next to its line, so that putting the sites in order reads
//! none of them. Sites in order are a [`SiteList`], whose lines are
//! written as they are.

use std::borrow::Borrow;
use std::cell::RefCell;
use std::cmp::{Ordering, Reverse};
use std::collections::HashSet;
use std::fmt;
use std::hash::{Hash, Hasher};

use plumb_format::Item;

use crate::site_item::tidy;
use crate::{canonical_domain, parent_domain, site, stored};
#[cfg(doc)]
use crate::{RecordSet, SITES_VERSION};

/// Sites' stored items keyed by domain; adding a site for a domain already
/// present merges them. Each site is held as its stored line (see the
/// module doc): reading one parses its line, so code that reads every site
/// more than once reads each once and keeps what it needs of it.
#[derive(Debug, Clone, Default)]
pub struct Sites {
    held: HashSet<Held>,
}

impl Sites {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Whether the set holds a site of `domain`.
    pub fn contains(&self, domain: &str) -> bool {
        self.held.contains(domain)
    }

    /// The site of `domain`, read from its line.
    pub fn get(&self, domain: &str) -> Option<Item> {
        self.held.get(domain).map(Held::item)
    }

    /// Changes the site of `domain` with `change`, which gets the site
    /// created with nothing but its domain ([`site::new`]) if it is not
    /// there yet, and keeps what `change` makes of it. Returns what
    /// `change` returns. `domain` must already be canonical, as
    /// [`crate::registrable_domain`] returns it, and `change` must leave the
    /// item's id as it is.
    pub fn update<R>(&mut self, domain: &str, change: impl FnOnce(&mut Item) -> R) -> R {
        let mut site = match self.held.take(domain) {
            Some(held) => held.item(),
            None => site::new(domain),
        };
        let changed = change(&mut site);
        self.held.insert(Held::new(domain, site));
        changed
    }

    /// [`Sites::update`] of a site the set holds: `None`, and nothing
    /// changed, when it holds none of `domain`.
    fn update_held<R>(&mut self, domain: &str, change: impl FnOnce(&mut Item) -> R) -> Option<R> {
        let mut site = self.held.take(domain)?.item();
        let changed = change(&mut site);
        self.held.insert(Held::new(domain, site));
        Some(changed)
    }

    /// Takes off parent domains what they got from `seed` items of their
    /// subdomain sites ([`parent_domain`]) when those were folded into them:
    /// names, Wikidata's description ([`site::wikidata_about`]) and the
    /// Wikipedia intro ([`site::intro`]). For seed data read again after
    /// the subdomain lists grew ([`SITES_VERSION`]), before it is upserted;
    /// the parent's own seed item adds its own back. The seed's text is
    /// compared cleaned, as the set holds it ([`site::clean`]). `seed` may
    /// be the items or references to them, or a [`SiteList`], whose sites
    /// on a subdomain alone are read.
    pub fn split_subdomain_sites<S: Borrow<Item>>(&mut self, seed: impl IntoIterator<Item = S>) {
        for subsite in seed {
            self.split_subdomain_site(subsite.borrow());
        }
    }

    /// [`Sites::split_subdomain_sites`] of one seed item.
    fn split_subdomain_site(&mut self, subsite: &Item) {
        // Whether the parent's text is there and is the seed's, cleaned.
        let same = |parent: Option<&str>, seed: Option<&str>| {
            parent.is_some() && parent == seed.map(str::to_string).and_then(tidy).as_deref()
        };
        let Some(parent) = parent_domain(site::domain(subsite)) else {
            return;
        };
        self.update_held(parent, |site| {
            let names: Vec<String> = subsite.names.iter().cloned().filter_map(tidy).collect();
            site.names.retain(|name| !names.contains(name));
            if same(site::wikidata_about(site), site::wikidata_about(subsite)) {
                site::set_wikidata_about(site, None);
            }
            if same(site::intro(site), site::intro(subsite)) {
                site::set_intro(site, None);
            }
        });
    }

    /// Takes back what a misread Wikidata claim (an email address read as a
    /// website) gave the site of `domain`: the item's `names`, and the facts
    /// official websites bring (whether it is one, its sitelinks, country,
    /// kinds, Wikidata's description and the intro), which the domain's
    /// own seed item adds back when it is upserted after. `names` are
    /// compared cleaned, as the set holds them ([`site::clean`]).
    pub fn take_back_official_site(&mut self, domain: &str, names: &[&str]) {
        self.update_held(domain, |site| {
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
        });
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
        let site = match self.held.take(site.id.as_str()) {
            Some(existing) => {
                let mut existing = existing.item();
                merge(&mut existing, site);
                existing
            }
            None => site,
        };
        self.held.insert(Held::of(site));
        true
    }

    /// Every site, in no particular order, each read from its line as it
    /// is reached.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Item> + '_ {
        self.held.iter().map(Held::item)
    }

    /// Keeps only the sites for which `keep` is true, reading each.
    pub fn retain(&mut self, mut keep: impl FnMut(&Item) -> bool) {
        self.held.retain(|held| keep(&held.item()));
        self.fit();
    }

    /// Each site's domain and link score ([`site::link_score`]), in no
    /// particular order, without reading any site.
    pub fn link_scores(&self) -> impl ExactSizeIterator<Item = (&str, f32)> + '_ {
        self.held.iter().map(|held| (held.domain(), held.score()))
    }

    /// Keeps only the sites of the domains for which `keep` is true,
    /// without reading any site.
    pub fn retain_domains(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.held.retain(|held| keep(held.domain()));
        self.fit();
    }

    /// Gives back the room of sites taken out, once most are: a table
    /// emptied in place also takes longer to look up in, each lookup
    /// probing past the places the sites took.
    fn fit(&mut self) {
        if self.held.len() < self.held.capacity() / 4 {
            self.held.shrink_to_fit();
        }
    }

    /// Every site, best [`site::link_score`] first, ties broken by domain,
    /// each read from its line as it is reached: the order of
    /// [`Sites::into_sorted`].
    pub fn by_link_score(&self) -> impl ExactSizeIterator<Item = Item> + '_ {
        let mut held: Vec<&Held> = self.held.iter().collect();
        held.sort_unstable_by(|a, b| best_first(a, b));
        held.into_iter().map(Held::item)
    }

    /// All sites, best [`site::link_score`] first, ties broken by domain,
    /// as [`crate::sort_by_link_score`] sorts records: their lines, moved
    /// and not read.
    pub fn into_sorted(self) -> SiteList {
        let mut held: Vec<Held> = self.held.into_iter().collect();
        held.sort_unstable_by(best_first);
        SiteList { held }
    }
}

/// All sites, in no particular order, each read from its line as it is
/// taken, and the line freed then, so a caller that keeps only a little of
/// each never holds two copies of the set.
impl IntoIterator for Sites {
    type Item = Item;
    type IntoIter = Box<dyn ExactSizeIterator<Item = Item> + Send>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.held.into_iter().map(|held| held.item()))
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

/// Sites in an order, each held as its stored line as [`Sites`] holds it:
/// what [`Sites::into_sorted`] gives, or sites collected in the order they
/// come. What it writes of them is their lines as they are
/// ([`SiteList::lines`]), and what it reads, one site at a time
/// ([`SiteList::iter`]).
#[derive(Debug, Clone, Default)]
pub struct SiteList {
    held: Vec<Held>,
}

impl SiteList {
    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Keeps the first `n` sites.
    pub fn truncate(&mut self, n: usize) {
        self.held.truncate(n);
        self.held.shrink_to_fit();
    }

    /// The sites in order, each read from its line as it is reached.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Item> + '_ {
        self.held.iter().map(Held::item)
    }

    /// The sites' stored lines in order, without their line breaks: what
    /// [`stored::write_item`] writes of each.
    pub fn lines(&self) -> impl ExactSizeIterator<Item = &[u8]> + Clone + '_ {
        self.held.iter().map(Held::line)
    }
}

/// The sites in order, each read from its line as it is taken, and the
/// line freed then.
impl IntoIterator for SiteList {
    type Item = Item;
    type IntoIter = Box<dyn ExactSizeIterator<Item = Item> + Send>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.held.into_iter().map(|held| held.item()))
    }
}

/// The sites in order, each read from its line as it is reached.
impl<'a> IntoIterator for &'a SiteList {
    type Item = Item;
    type IntoIter = Box<dyn ExactSizeIterator<Item = Item> + Send + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.held.iter().map(Held::item))
    }
}

/// The sites in the order they come, each as its line, neither merged nor
/// cleaned.
impl FromIterator<Item> for SiteList {
    fn from_iter<I: IntoIterator<Item = Item>>(iter: I) -> Self {
        let held = iter.into_iter().map(Held::of).collect();
        SiteList { held }
    }
}

/// A site as [`Sites`] and [`SiteList`] hold it, in one allocation: its
/// link score (4 bytes), the length of its domain (4 bytes), its domain,
/// and its stored line ([`stored::write_item`]). The domain is the key a
/// [`Sites`] finds it by, and the score orders it, both without reading
/// the line.
#[derive(Clone)]
struct Held(Box<[u8]>);

/// Bytes before a [`Held`] site's domain: its score and the domain's
/// length.
const HEAD: usize = 8;

thread_local! {
    /// Where [`Held::new`] writes a line before it knows its length, so
    /// that each site takes one allocation of just its size.
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

impl Held {
    /// `site`, held under `domain`, as [`stored::write_item`] writes it.
    /// Its score is the site's link score: a site written cleaned, for an
    /// edge that would not read back, has its signals as they were, and
    /// so its score.
    ///
    /// The item is dropped before the line is allocated, so that the line
    /// can take the room the item's parts leave: allocated while they were
    /// still there, lines and freed parts ended up spread through the heap,
    /// and loading 500,000 crawled sites took 190 s rather than 130 s.
    fn new(domain: &str, site: Item) -> Held {
        Held::write(|bytes| {
            Held::put(bytes, domain, &site);
            drop(site);
        })
    }

    /// [`Held::new`] under the site's own domain, its id.
    fn of(site: Item) -> Held {
        Held::write(|bytes| {
            Held::put(bytes, &site.id, &site);
            drop(site);
        })
    }

    /// Writes what [`Held::new`] holds of `site` to `bytes`.
    fn put(bytes: &mut Vec<u8>, domain: &str, site: &Item) {
        bytes.extend(site::link_score(site).to_le_bytes());
        let len = u32::try_from(domain.len()).expect("a domain shorter than 4 GB");
        bytes.extend(len.to_le_bytes());
        bytes.extend(domain.as_bytes());
        stored::write_item(&mut *bytes, site).expect("a site is written as JSON");
    }

    /// The site `write` writes to the scratch buffer, in an allocation of
    /// just its size.
    fn write(write: impl FnOnce(&mut Vec<u8>)) -> Held {
        SCRATCH.with(|scratch| {
            let mut bytes = scratch.borrow_mut();
            bytes.clear();
            write(&mut bytes);
            let held = Held(Box::from(&bytes[..]));
            // A big line's room is not kept for the next one.
            if bytes.capacity() > 1 << 16 {
                *bytes = Vec::new();
            }
            held
        })
    }

    fn score(&self) -> f32 {
        f32::from_le_bytes(self.0[..4].try_into().expect("four bytes"))
    }

    fn domain_bytes(&self) -> &[u8] {
        let len = u32::from_le_bytes(self.0[4..HEAD].try_into().expect("four bytes"));
        &self.0[HEAD..HEAD + len as usize]
    }

    fn domain(&self) -> &str {
        std::str::from_utf8(self.domain_bytes()).expect("a domain is UTF-8")
    }

    fn line(&self) -> &[u8] {
        &self.0[HEAD + self.domain_bytes().len()..]
    }

    /// The site, read from its line.
    fn item(&self) -> Item {
        // The line is the JSON of an item whose edges all read
        // ([`stored::write_item`]), so it reads back as that item.
        serde_json::from_slice(self.line()).expect("a held site reads back")
    }
}

impl fmt::Debug for Held {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Held")
            .field(&String::from_utf8_lossy(self.line()))
            .finish()
    }
}

/// Held sites are found by their domains.
impl Borrow<str> for Held {
    fn borrow(&self) -> &str {
        self.domain()
    }
}

impl Hash for Held {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.domain().hash(state);
    }
}

impl PartialEq for Held {
    fn eq(&self, other: &Held) -> bool {
        self.domain_bytes() == other.domain_bytes()
    }
}

impl Eq for Held {}

/// Best link score first, ties broken by domain, as
/// [`crate::sort_by_link_score`] sorts records.
fn best_first(a: &Held, b: &Held) -> Ordering {
    b.score()
        .total_cmp(&a.score())
        .then_with(|| a.domain_bytes().cmp(b.domain_bytes()))
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
            assert_eq!(site, record.to_stored_item(), "{what}: {domain}");
            let line = serde_json::to_vec(&site).unwrap();
            assert_eq!(line, stored::line(record).unwrap(), "{what}: {domain}");
            let held = sites.held.get(domain.as_str()).unwrap();
            assert_eq!(held.line(), line, "{what}: {domain}");
            assert_eq!(held.score(), record.link_score(), "{what}: {domain}");
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
        assert_eq!(checked.get("usbank.com"), Some(site::new("usbank.com")));

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
        assert_eq!(site::headings(&rust), ["Why Rust?"]);
        assert_eq!(sites.len(), 6);
    }

    #[test]
    fn an_update_is_of_the_site_or_a_new_one_as_an_entry_is_with_records() {
        let (mut records, mut sites) = both(&incoming());
        for domain in ["python.org", "plumbsearch.org"] {
            records.entry(domain).add_alias("Plumb Search");
            sites.update(domain, |site| site::add_alias(site, "Plumb Search"));
        }
        records.entry("rust-lang.org");
        let new = sites.update("rust-lang.org", |site| site.clone());
        assert_eq!(new, site::new("rust-lang.org"));
        assert!(sites.contains("rust-lang.org"));
        assert!(!sites.contains("crates.io"));
        assert_same(&sites, &records, "update");
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
        let parents = [parent, google, full_record()];
        let (mut records, mut sites) = both(&parents);

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
        assert_eq!(site::wikidata_about(&yc), None);
        assert_eq!(site::intro(&yc), None);
        let google = sites.get("google.com").unwrap();
        assert_eq!(site::wikidata_about(&google), Some("search engine"));

        // The same of seed sites held as lines.
        let mut split = both(&parents).1;
        let lines: SiteList = seed_items.iter().cloned().collect();
        split.split_subdomain_sites(&lines);
        for domain in ["ycombinator.com", "google.com", "python.org"] {
            assert_eq!(split.get(domain), sites.get(domain), "{domain}");
        }

        records.extend(seed);
        sites.extend(seed_items);
        assert_same(&sites, &records, "split, then the seed");
        split.extend(lines);
        assert_same(&split, &records, "split from lines, then the seed");
        let yc = sites.get("ycombinator.com").unwrap();
        assert_eq!(site::wikidata_about(&yc), Some("startup accelerator"));
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
        let signals = site::signals(&python);
        assert!(!signals.official_site);
        assert_eq!(signals.sitelinks, 0);
        assert_eq!(signals.tranco_rank, Some(400));
        assert!(site::kinds(&python).is_empty());
        assert_eq!(site::country(&python), None);
        assert_eq!(site::wikidata_about(&python), None);
        assert_eq!(site::intro(&python), None);
        assert!(site::summary(&python).is_some());
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
        let in_order: Vec<Item> = all_sites.by_link_score().collect();
        let scores: Vec<(String, f32)> = all_sites
            .link_scores()
            .map(|(domain, score)| (domain.to_string(), score))
            .collect();
        let list = all_sites.into_sorted();
        let sites: Vec<Item> = list.iter().collect();
        assert_eq!(sites, in_order);
        assert_eq!(list.clone().into_iter().collect::<Vec<Item>>(), sites);
        assert_eq!((&list).into_iter().collect::<Vec<Item>>(), sites);
        assert_eq!(scores.len(), sites.len());
        for (domain, score) in scores {
            let site = sites.iter().find(|site| site.id == domain).unwrap();
            assert_eq!(score, site::link_score(site), "{domain}");
        }
        // Written, the list is its sites' stored lines, as they are.
        let lines: Vec<&[u8]> = list.lines().collect();
        for (line, record) in lines.iter().zip(&records) {
            assert_eq!(line.to_vec(), stored::line(record).unwrap());
        }
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
    fn a_site_is_held_as_its_stored_line() {
        let (_, sites) = both(&incoming());
        for site in sites.iter() {
            let held = sites.held.get(site.id.as_str()).unwrap();
            assert_eq!(held.domain(), site.id);
            assert_eq!(held.line(), stored::item_as_line(&site).unwrap());
            assert_eq!(held.score(), site::link_score(&site));
        }
        // A site known only by its ranks takes a line of under 100 bytes,
        // and is held in one allocation of the line, its domain and 8
        // bytes more.
        let mut ranked = SiteRecord::new("example.com");
        ranked.signals.tranco_rank = Some(1_234);
        ranked.signals.harmonic_rank = Some(56_789);
        let line = stored::line(&ranked).unwrap();
        assert!(line.len() < 100, "{}", String::from_utf8_lossy(&line));
        let held = Held::new("example.com", ranked.to_stored_item());
        assert_eq!(held.line(), line);
        assert_eq!(held.0.len(), HEAD + "example.com".len() + line.len());

        // A change that leaves an edge no reader takes holds the site as it
        // is written, cleaned, as before it held it until it was written.
        let mut sites = Sites::new();
        let changed = sites.update("example.com", |site| {
            site::set_links_to(site, vec!["foo_bar.blogspot.com".into(), "a.com".into()]);
            site.clone()
        });
        let held = sites.get("example.com").unwrap();
        assert_eq!(site::links_to(&held), ["a.com"]);
        assert_eq!(
            serde_json::to_vec(&held).unwrap(),
            stored::item_as_line(&changed).unwrap()
        );
    }

    #[test]
    fn a_list_keeps_its_sites_in_the_order_they_come() {
        let (_, sites) = both(&incoming());
        let mut items: Vec<Item> = sites.iter().collect();
        items.sort_by(|a, b| b.id.cmp(&a.id));
        let mut list: SiteList = items.iter().cloned().collect();
        assert_eq!(list.len(), items.len());
        assert_eq!(list.iter().collect::<Vec<Item>>(), items);
        for (line, site) in list.lines().zip(&items) {
            assert_eq!(line, stored::item_as_line(site).unwrap());
        }
        list.truncate(2);
        assert_eq!(list.into_iter().collect::<Vec<Item>>(), items[..2]);
        assert!(SiteList::default().is_empty());
    }

    #[test]
    fn sites_are_kept_by_domain_without_reading_them() {
        let (_, mut sites) = both(&incoming());
        let before: Vec<Item> = sites.by_link_score().collect();
        sites.retain_domains(|domain| domain != "example.com" && domain != "usbank.com");
        let after: Vec<Item> = sites.by_link_score().collect();
        let kept: Vec<Item> = before
            .into_iter()
            .filter(|site| site.id != "example.com" && site.id != "usbank.com")
            .collect();
        assert_eq!(after, kept);
        assert_eq!(sites.len(), 2);
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
