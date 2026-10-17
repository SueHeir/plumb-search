//! A node's sites as their stored items ([`crate::site`]), by domain: a
//! [`Sites`]. Adding a site that is there already merges the two
//! ([`site::merge`]), and seed data read again takes back what a subdomain
//! site or a misread Wikidata claim gave a site.
//!
//! A set holds each site as its stored line ([`stored::write_item`]), not
//! as its item. An item is a wide struct, and each JSON object in it (its
//! fields, and every object among them: its ranks, crawl marks, each link
//! text) is a B-tree node of over 600 bytes however little it holds. So a
//! site known only by its ranks takes about 2.1 kB as an item and 0.15 kB
//! as its line, and a crawled site about 12 kB as an item and a little
//! over its line's 1.6 kB as its line. A site is read from its line when
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
#[cfg(doc)]
use crate::SITES_VERSION;
use crate::{canonical_domain, parent_domain, site, stored};

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
        self.best_first().into_iter().map(Held::item)
    }

    /// Every site's stored line, without its line break, in no particular
    /// order and without reading any site: what [`stored::write_item`]
    /// writes of it.
    pub fn lines(&self) -> impl ExactSizeIterator<Item = &[u8]> + '_ {
        self.held.iter().map(Held::line)
    }

    /// Every site's stored line, in the order of [`Sites::by_link_score`],
    /// as [`SiteList::lines`] gives the lines of [`Sites::into_sorted`]:
    /// for writing the set while keeping it, without reading any site or
    /// copying any line.
    pub fn lines_by_link_score(&self) -> impl ExactSizeIterator<Item = &[u8]> + '_ {
        self.best_first().into_iter().map(Held::line)
    }

    fn best_first(&self) -> Vec<&Held> {
        let mut held: Vec<&Held> = self.held.iter().collect();
        held.sort_unstable_by(|a, b| best_first(a, b));
        held
    }

    /// All sites, best [`site::link_score`] first, ties broken by domain
    /// ([`sort_by_link_score`]): their lines, moved and not read.
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
    /// A list of no sites.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// Adds `site` at the end, as its line.
    pub fn push(&mut self, site: Item) {
        self.held.push(Held::of(site));
    }

    /// Adds a site held as its line at the end, as it is.
    pub fn push_line(&mut self, site: SiteLine) {
        self.held.push(site.0);
    }

    /// The sites' domains in order, read without their lines.
    pub fn domains(&self) -> impl ExactSizeIterator<Item = &str> + '_ {
        self.held.iter().map(Held::domain)
    }

    /// Keeps the sites `keep` says to, in order, each read from its line;
    /// the lines kept are not written again.
    pub fn retain(&mut self, mut keep: impl FnMut(&Item) -> bool) {
        self.held.retain(|held| keep(&held.item()));
    }

    /// Keeps only the sites of the domains for which `keep` is true, in
    /// order, without reading any site.
    pub fn retain_domains(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.held.retain(|held| keep(held.domain()));
    }

    /// Keeps the sites `keep` says to, in order, each read from its line
    /// and written again as `keep` leaves it.
    pub fn retain_mut(&mut self, mut keep: impl FnMut(&mut Item) -> bool) {
        let held = std::mem::take(&mut self.held);
        self.held = held
            .into_iter()
            .filter_map(|held| {
                let mut site = held.item();
                drop(held);
                keep(&mut site).then(|| Held::of(site))
            })
            .collect();
    }

    /// The sites `first` says are its, and the rest, each in order, their
    /// lines moved and not written again.
    pub fn partition(self, mut first: impl FnMut(&Item) -> bool) -> (SiteList, SiteList) {
        let (yes, no) = self.held.into_iter().partition(|held| first(&held.item()));
        (SiteList { held: yes }, SiteList { held: no })
    }

    /// Sorts the sites by `key`, read off each site's item once; stable,
    /// and the lines are moved, not written again.
    pub fn sort_by_cached_key<K: Ord>(&mut self, mut key: impl FnMut(&Item) -> K) {
        self.held.sort_by_cached_key(|held| key(&held.item()));
    }

    /// Splits the list in two at `at`: the sites from `at` on are returned,
    /// and the first `at` stay.
    pub fn split_off(&mut self, at: usize) -> SiteList {
        SiteList {
            held: self.held.split_off(at),
        }
    }

    /// The sites in order, each held as its line.
    pub fn into_lines(self) -> impl ExactSizeIterator<Item = SiteLine> {
        self.held.into_iter().map(SiteLine)
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

/// Sites held as their lines, in the order they come.
impl FromIterator<SiteLine> for SiteList {
    fn from_iter<I: IntoIterator<Item = SiteLine>>(iter: I) -> Self {
        let held = iter.into_iter().map(|site| site.0).collect();
        SiteList { held }
    }
}

/// Adds the sites at the end, each as its line.
impl Extend<Item> for SiteList {
    fn extend<I: IntoIterator<Item = Item>>(&mut self, iter: I) {
        self.held.extend(iter.into_iter().map(Held::of));
    }
}

/// Adds another list's sites at the end, their lines as they are.
impl Extend<SiteLine> for SiteList {
    fn extend<I: IntoIterator<Item = SiteLine>>(&mut self, iter: I) {
        self.held.extend(iter.into_iter().map(|site| site.0));
    }
}

/// Two lists are the same when they hold the same lines in the same order.
impl PartialEq for SiteList {
    fn eq(&self, other: &SiteList) -> bool {
        self.held.len() == other.held.len()
            && self.held.iter().zip(&other.held).all(|(a, b)| a.0 == b.0)
    }
}

impl Eq for SiteList {}

/// One site held as a [`SiteList`] holds each of its sites: its stored
/// line ([`stored::write_item`]), with its domain, read as its item when
/// asked for ([`SiteLine::item`]). Code that keeps sites one by one, such
/// as the sites a network search found, keeps them this way and not as
/// items, which take several times the memory (see the module doc).
#[derive(Clone)]
pub struct SiteLine(Held);

impl SiteLine {
    /// `site` as its line.
    pub fn of(site: Item) -> SiteLine {
        SiteLine(Held::of(site))
    }

    /// The site's domain, read without its line.
    pub fn domain(&self) -> &str {
        self.0.domain()
    }

    /// The site, read from its line.
    pub fn item(&self) -> Item {
        self.0.item()
    }

    /// The site's stored line, without its line break.
    pub fn line(&self) -> &[u8] {
        self.0.line()
    }

    /// Changes the site with `change`, read from its line and written
    /// again as `change` leaves it; returns what `change` does.
    pub fn update<T>(&mut self, change: impl FnOnce(&mut Item) -> T) -> T {
        let mut site = self.item();
        let out = change(&mut site);
        *self = SiteLine::of(site);
        out
    }
}

impl From<Item> for SiteLine {
    fn from(site: Item) -> SiteLine {
        SiteLine::of(site)
    }
}

impl fmt::Debug for SiteLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SiteLine")
            .field(&String::from_utf8_lossy(self.line()))
            .finish()
    }
}

/// Two sites held are the same when their lines are.
impl PartialEq for SiteLine {
    fn eq(&self, other: &SiteLine) -> bool {
        self.0 .0 == other.0 .0
    }
}

impl Eq for SiteLine {}

/// Serialized as its item.
impl serde::Serialize for SiteLine {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&self.item(), serializer)
    }
}

impl<'de> serde::Deserialize<'de> for SiteLine {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <Item as serde::Deserialize>::deserialize(deserializer).map(SiteLine::of)
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

/// Best link score first, ties broken by domain, as [`sort_by_link_score`]
/// sorts.
fn best_first(a: &Held, b: &Held) -> Ordering {
    b.score()
        .total_cmp(&a.score())
        .then_with(|| a.domain_bytes().cmp(b.domain_bytes()))
}

/// Sorts best [`site::link_score`] first, ties broken by domain so output
/// is stable. Each score is
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
/// sites are sorted, so that a sort can keep it as its key.
fn total_order(score: f32) -> i32 {
    let bits = score.to_bits() as i32;
    bits ^ (((bits >> 31) as u32) >> 1) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::site::tests::{full_site, variants};
    use crate::Signals;

    /// Checks that `sites` holds each of its sites as its stored line, with
    /// its link score beside it.
    fn assert_held(sites: &Sites, what: &str) {
        assert_eq!(sites.iter().count(), sites.len(), "{what}");
        assert_eq!(sites.is_empty(), sites.iter().next().is_none(), "{what}");
        for site in sites.iter() {
            let domain = site.id.as_str();
            let held = sites.held.get(domain).unwrap();
            let line = stored::item_as_line(&site).unwrap();
            assert_eq!(held.line(), line, "{what}: {domain}");
            assert_eq!(held.score(), site::link_score(&site), "{what}: {domain}");
            assert_eq!(sites.get(domain), Some(site.clone()), "{what}: {domain}");
        }
    }

    /// The stored lines of `sites`, in the order of their domains.
    fn lines(sites: &Sites) -> Vec<String> {
        let mut lines: Vec<String> = sites
            .lines()
            .map(|line| String::from_utf8(line.to_vec()).unwrap())
            .collect();
        lines.sort();
        lines
    }

    fn ranked(domain: &str, signals: Signals) -> Item {
        let mut site = site::new(domain);
        site::set_signals(&mut site, &signals);
        site
    }

    /// Sites as a node takes them in, not all clean: python.org seven ways
    /// (the variants merging is tested on) and once more with text and a
    /// URL to clean; domains not written in their canonical form; a bot
    /// check read in place of a homepage, then the homepage; and a domain
    /// that is none.
    fn incoming() -> Vec<Item> {
        let mut sites = variants();
        let mut dirty = full_site();
        dirty.url = Some("HTTPS://WWW.Python.org/index%2ehtml?".into());
        dirty.title = Some("  Welcome\u{7} to Python ".into());
        dirty.names.push(" Python".into());
        dirty.names.push("Dư\u{323} ba\u{301}o".into());
        sites.push(dirty);
        let tranco = Signals {
            tranco_rank: Some(10),
            ..Signals::default()
        };
        sites.push(ranked("Example.COM", tranco));
        let mut example = site::new("example.com");
        site::add_alias(&mut example, "Example");
        sites.push(example);
        let mut munich = site::new("münchen.de");
        munich.title = Some("München".into());
        sites.push(munich);
        let harmonic = Signals {
            harmonic_rank: Some(5_000),
            ..Signals::default()
        };
        let mut munich = ranked("xn--mnchen-3ya.de", harmonic);
        munich.names = vec!["Munich".into()];
        sites.push(munich);
        let mut check = site::new("usbank.com");
        check.url = Some("https://www.google.com/sorry/index".into());
        check.title = Some("Sorry...".into());
        site::set_headings(&mut check, vec!["About this page".into()]);
        site::set_crawled(&mut check, Some(1_791_605_520));
        sites.push(check);
        let mut bank = site::new("usbank.com");
        bank.url = Some("https://www.usbank.com/".into());
        bank.title = Some("U.S. Bank".into());
        site::set_crawled(&mut bank, Some(1_791_700_000));
        sites.push(bank);
        sites.push(site::new("not a domain"));
        sites
    }

    /// Crawls other nodes shared, without page text or headings: of
    /// python.org, of rust-lang.org, which this node crawled itself before,
    /// and of a site not yet in the set.
    fn shared() -> Vec<Item> {
        let mut python = variants()[5].clone();
        assert_eq!(python.text, None);
        site::set_crawled(&mut python, Some(1_791_950_000));
        let mut rust = variants()[5].clone();
        rust.id = "rust-lang.org".into();
        rust.url = Some("https://www.rust-lang.org/".into());
        rust.title = Some("Rust Programming Language".into());
        let mut crates = rust.clone();
        crates.id = "crates.io".into();
        crates.url = Some("https://crates.io/".into());
        crates.title = Some("crates.io: Rust Package Registry".into());
        vec![python, rust, crates]
    }

    /// The set the sites make, upserted one at a time.
    fn upserted(sites: &[Item]) -> Sites {
        let mut set = Sites::new();
        for site in sites {
            let taken = set.upsert(site.clone());
            assert_eq!(taken, site.id != "not a domain", "{}", site.id);
            assert_held(&set, &format!("upsert {}", site.id));
        }
        set
    }

    #[test]
    fn sites_upsert_cleaned_and_merged() {
        let mut sites = upserted(&incoming());
        assert_eq!(
            lines(&sites),
            [
                r#"{"id":"example.com","kind":"site","names":["Example"],"fields":{"ranks":{"tranco":10}}}"#,
                r#"{"id":"python.org","kind":"site","title":"Python","url":"https://python.org/","names":["Python","Python Software Foundation","Dự báo"],"about":"The official home of the Python Programming Language","lang":"en","weight":500.0,"edges":[{"rel":"links_to","to":"site:example.com"}],"fields":{"crawl":{"failures":1,"tried":1791900000},"crawl_version":8,"crawled":"2026-10-12T10:13:20Z","fact_checks":{"agreed":3,"checked":20},"icon":"iVBORw0KGgo=","intro":"Python is a high-level, general-purpose programming language.","kinds":["programming language"],"link_texts":[{"count":40,"linkers":"00a1b2c3d4e5f617","text":"python"},{"count":9,"text":"py"},{"count":3,"text":"python.org"}],"news":[{"at":1791605520,"title":"Python 3.15 is out","url":"https://www.python.org/news/"}],"ranks":{"harmonic":100,"pagerank":250,"tranco":400},"redirect":{"at":"2026-10-13T14:00:00Z","to":"foo_bar.com"},"summary":"The home of the Python programming language.","wikidata":{"about":"general-purpose programming language","country":"US","official_site":true,"sitelinks":150}}}"#,
                r#"{"id":"usbank.com","kind":"site","title":"U.S. Bank","url":"https://www.usbank.com/","fields":{"crawled":"2026-10-11T06:26:40Z"}}"#,
                r#"{"id":"xn--mnchen-3ya.de","kind":"site","title":"München","names":["Munich"],"fields":{"ranks":{"harmonic":5000}}}"#,
            ]
        );
        assert!(!sites.upsert(site::new("co.uk")));
        // Of the bot check alone, nothing but the domain is left.
        let check = incoming()
            .into_iter()
            .find(|site| site.title.as_deref() == Some("Sorry..."));
        let mut checked = Sites::new();
        assert!(checked.upsert(check.unwrap()));
        assert_eq!(checked.get("usbank.com"), Some(site::new("usbank.com")));

        let mut rust = site::new("rust-lang.org");
        rust.text = Some("A language empowering everyone.".into());
        site::set_headings(&mut rust, vec!["Why Rust?".into()]);
        site::set_crawled(&mut rust, Some(1_791_600_000));
        assert!(sites.upsert(rust));
        for site in shared() {
            let domain = site.id.clone();
            assert!(sites.upsert_shared(site));
            assert_held(&sites, &format!("upsert_shared {domain}"));
        }
        // What the fresher shared crawl left empty stays.
        assert_eq!(
            lines(&sites),
            [
                r#"{"id":"crates.io","kind":"site","title":"crates.io: Rust Package Registry","url":"https://crates.io/","edges":[{"rel":"links_to","to":"site:example.com"}],"fields":{"crawl":{"failures":1,"tried":1791900000},"crawl_version":8,"crawled":"2026-10-12T10:13:20Z","icon":"R0lGODlh","kinds":["website"],"news":[{"at":1791800000,"title":"Python 3.15","url":"https://www.python.org/news/315"}]}}"#,
                r#"{"id":"example.com","kind":"site","names":["Example"],"fields":{"ranks":{"tranco":10}}}"#,
                r#"{"id":"python.org","kind":"site","title":"Python","url":"https://python.org/","names":["Python","Python Software Foundation","Dự báo"],"about":"The official home of the Python Programming Language","lang":"en","weight":500.0,"edges":[{"rel":"links_to","to":"site:example.com"}],"fields":{"crawl":{"failures":1,"tried":1791900000},"crawl_version":8,"crawled":"2026-10-14T03:53:20Z","fact_checks":{"agreed":3,"checked":20},"icon":"iVBORw0KGgo=","intro":"Python is a high-level, general-purpose programming language.","kinds":["programming language"],"link_texts":[{"count":40,"linkers":"00a1b2c3d4e5f617","text":"python"},{"count":9,"text":"py"},{"count":3,"text":"python.org"}],"news":[{"at":1791605520,"title":"Python 3.15 is out","url":"https://www.python.org/news/"}],"ranks":{"harmonic":100,"pagerank":250,"tranco":400},"summary":"The home of the Python programming language.","wikidata":{"about":"general-purpose programming language","country":"US","official_site":true,"sitelinks":150}}}"#,
                r#"{"id":"rust-lang.org","kind":"site","title":"Rust Programming Language","url":"https://www.rust-lang.org/","text":"A language empowering everyone.","edges":[{"rel":"links_to","to":"site:example.com"}],"fields":{"crawl":{"failures":1,"tried":1791900000},"crawl_version":8,"crawled":"2026-10-12T10:13:20Z","headings":["Why Rust?"]}}"#,
                r#"{"id":"usbank.com","kind":"site","title":"U.S. Bank","url":"https://www.usbank.com/","fields":{"crawled":"2026-10-11T06:26:40Z"}}"#,
                r#"{"id":"xn--mnchen-3ya.de","kind":"site","title":"München","names":["Munich"],"fields":{"ranks":{"harmonic":5000}}}"#,
            ]
        );
    }

    #[test]
    fn an_update_is_of_the_site_or_a_new_one() {
        let mut sites = upserted(&incoming());
        for domain in ["python.org", "plumbsearch.org"] {
            sites.update(domain, |site| site::add_alias(site, "Plumb Search"));
        }
        let new = sites.update("rust-lang.org", |site| site.clone());
        assert_eq!(new, site::new("rust-lang.org"));
        assert!(sites.contains("rust-lang.org"));
        assert!(!sites.contains("crates.io"));
        assert_held(&sites, "update");
        assert_eq!(
            lines(&sites),
            [
                r#"{"id":"example.com","kind":"site","names":["Example"],"fields":{"ranks":{"tranco":10}}}"#,
                r#"{"id":"plumbsearch.org","kind":"site","names":["Plumb Search"]}"#,
                r#"{"id":"python.org","kind":"site","title":"Python","url":"https://python.org/","names":["Python","Python Software Foundation","Dự báo","Plumb Search"],"about":"The official home of the Python Programming Language","lang":"en","weight":500.0,"edges":[{"rel":"links_to","to":"site:example.com"}],"fields":{"crawl":{"failures":1,"tried":1791900000},"crawl_version":8,"crawled":"2026-10-12T10:13:20Z","fact_checks":{"agreed":3,"checked":20},"icon":"iVBORw0KGgo=","intro":"Python is a high-level, general-purpose programming language.","kinds":["programming language"],"link_texts":[{"count":40,"linkers":"00a1b2c3d4e5f617","text":"python"},{"count":9,"text":"py"},{"count":3,"text":"python.org"}],"news":[{"at":1791605520,"title":"Python 3.15 is out","url":"https://www.python.org/news/"}],"ranks":{"harmonic":100,"pagerank":250,"tranco":400},"redirect":{"at":"2026-10-13T14:00:00Z","to":"foo_bar.com"},"summary":"The home of the Python programming language.","wikidata":{"about":"general-purpose programming language","country":"US","official_site":true,"sitelinks":150}}}"#,
                r#"{"id":"rust-lang.org","kind":"site"}"#,
                r#"{"id":"usbank.com","kind":"site","title":"U.S. Bank","url":"https://www.usbank.com/","fields":{"crawled":"2026-10-11T06:26:40Z"}}"#,
                r#"{"id":"xn--mnchen-3ya.de","kind":"site","title":"München","names":["Munich"],"fields":{"ranks":{"harmonic":5000}}}"#,
            ]
        );
    }

    #[test]
    fn sites_upsert_and_sort() {
        let mut sites = Sites::new();
        let rank = |tranco| Signals {
            tranco_rank: Some(tranco),
            ..Signals::default()
        };
        sites.upsert(ranked("a.com", rank(50)));
        sites.upsert(ranked("b.com", rank(5)));
        sites.update("a.com", |site| site::add_alias(site, "Alpha"));
        sites.upsert(site::new("c.com"));
        assert_eq!(sites.len(), 3);
        let sorted: Vec<Item> = sites.into_sorted().into_iter().collect();
        let domains: Vec<&str> = sorted.iter().map(|site| site.id.as_str()).collect();
        assert_eq!(domains, ["b.com", "a.com", "c.com"]);
        assert_eq!(sorted[1].names, ["Alpha"]);
    }

    #[test]
    fn subdomain_sites_take_their_names_off_their_parent() {
        let mut parent = site::new("ycombinator.com");
        parent.names = vec!["Y Combinator".into(), "Hacker News".into()];
        site::set_wikidata_about(&mut parent, Some("social news website".into()));
        let intro = "Hacker News is a social news website.";
        site::set_intro(&mut parent, Some(intro.into()));
        let mut google = site::new("google.com");
        google.names = vec!["Google".into()];
        site::set_wikidata_about(&mut google, Some("search engine".into()));
        let parents = [parent, google, full_site()];
        let mut sites = upserted(&parents);

        // Seed text not yet cleaned, as the set holds it.
        let mut news = site::new("news.ycombinator.com");
        news.names = vec!["Hacker News ".into()];
        site::set_wikidata_about(&mut news, Some("social news\twebsite".into()));
        let intro = "Hacker News is a social\u{7}news website.";
        site::set_intro(&mut news, Some(intro.into()));
        let mut yc = site::new("ycombinator.com");
        yc.names = vec!["Y Combinator".into()];
        site::set_wikidata_about(&mut yc, Some("startup accelerator".into()));
        // A subdomain site whose parent has nothing of it.
        let mut scholar = site::new("scholar.google.com");
        scholar.names = vec!["Google Scholar".into()];
        site::set_wikidata_about(&mut scholar, Some("academic search engine".into()));
        let seed = vec![news, yc, scholar];

        sites.split_subdomain_sites(&seed);
        assert_held(&sites, "split");
        let yc = sites.get("ycombinator.com").unwrap();
        assert_eq!(yc.names, ["Y Combinator"]);
        assert_eq!(site::wikidata_about(&yc), None);
        assert_eq!(site::intro(&yc), None);
        let google = sites.get("google.com").unwrap();
        assert_eq!(google.names, ["Google"]);
        assert_eq!(site::wikidata_about(&google), Some("search engine"));
        assert_eq!(sites.get("python.org"), Some(full_site()));

        // The same of seed sites held as lines.
        let mut split = upserted(&parents);
        let list: SiteList = seed.iter().cloned().collect();
        split.split_subdomain_sites(&list);
        for domain in ["ycombinator.com", "google.com", "python.org"] {
            assert_eq!(split.get(domain), sites.get(domain), "{domain}");
        }

        sites.extend(seed);
        assert_held(&sites, "split, then the seed");
        split.extend(list);
        assert_held(&split, "split from lines, then the seed");
        assert_eq!(lines(&split), lines(&sites));
        assert_eq!(sites.len(), 5);
        let yc = sites.get("ycombinator.com").unwrap();
        assert_eq!(site::wikidata_about(&yc), Some("startup accelerator"));
        let news = sites.get("news.ycombinator.com").unwrap();
        assert_eq!(news.names, ["Hacker News"]);
        assert_eq!(site::wikidata_about(&news), Some("social news website"));
        assert_eq!(
            site::intro(&news),
            Some("Hacker News is a social news website.")
        );
    }

    #[test]
    fn an_official_site_is_taken_back() {
        let mut sites = upserted(&incoming());
        let names = ["Python Software Foundation ", "Nothing"];
        for domain in ["python.org", "example.com", "plumbsearch.org"] {
            sites.take_back_official_site(domain, &names);
        }
        assert_held(&sites, "take_back_official_site");
        assert_eq!(sites.len(), 4);
        let python = sites.get("python.org").unwrap();
        assert_eq!(python.names, ["Python", "Dự báo"]);
        let signals = site::signals(&python);
        assert!(!signals.official_site);
        assert_eq!(signals.sitelinks, 0);
        assert_eq!(signals.tranco_rank, Some(400));
        assert!(site::kinds(&python).is_empty());
        assert_eq!(site::country(&python), None);
        assert_eq!(site::wikidata_about(&python), None);
        assert_eq!(site::intro(&python), None);
        assert!(site::summary(&python).is_some());
        assert!(!python.fields.contains_key("wikidata"));
        let example = sites.get("example.com").unwrap();
        assert_eq!(example.names, ["Example"]);
    }

    #[test]
    fn sites_are_kept_and_sorted() {
        let mut sites = upserted(&incoming());
        // Sites without a score, which sort by domain.
        for domain in ["rust-lang.org", "plumbsearch.org"] {
            sites.upsert(site::new(domain));
        }
        let all_sites = sites.clone();

        sites.retain(|site| site.id != "example.com");
        assert_held(&sites, "retain by domain");
        assert_eq!(sites.len(), 5);
        sites.retain(|site| site.title.is_none() || site::link_score(site) > 0.5);
        assert_held(&sites, "retain by title and score");
        let mut kept: Vec<String> = sites.iter().map(|site| site.id).collect();
        kept.sort();
        assert_eq!(kept, ["plumbsearch.org", "python.org", "rust-lang.org"]);

        let in_order: Vec<Item> = all_sites.by_link_score().collect();
        let scores: Vec<(String, f32)> = all_sites
            .link_scores()
            .map(|(domain, score)| (domain.to_string(), score))
            .collect();
        let whole = all_sites.clone();
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
        // Written, the list is its sites' stored lines, as they are, and
        // so are the set's, in the same order.
        let lines: Vec<&[u8]> = list.lines().collect();
        let set_lines: Vec<&[u8]> = whole.lines_by_link_score().collect();
        assert_eq!(set_lines, lines);
        let mut unordered: Vec<&[u8]> = whole.lines().collect();
        unordered.sort_unstable();
        let mut ordered = lines.clone();
        ordered.sort_unstable();
        assert_eq!(unordered, ordered);
        for (line, site) in lines.iter().zip(&sites) {
            assert_eq!(line.to_vec(), stored::item_as_line(site).unwrap());
        }
        let domains: Vec<&str> = sites.iter().map(|site| site.id.as_str()).collect();
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
        let mut sorted: Vec<Item> = sites.iter().rev().cloned().collect();
        sort_by_link_score(&mut sorted);
        assert_eq!(sorted, sites);
    }

    #[test]
    fn sites_collect_and_extend_as_they_upsert() {
        let all = incoming();
        let (first, rest) = all.split_at(5);
        let mut sites: Sites = first.iter().cloned().collect();
        assert_held(&sites, "collect");
        assert_eq!(lines(&sites), lines(&upserted(first)));
        sites.extend(rest.iter().cloned());
        assert_held(&sites, "extend");
        let one_at_a_time = upserted(&all);
        assert_eq!(lines(&sites), lines(&one_at_a_time));

        let mut taken: Vec<Item> = sites.into_iter().collect();
        taken.sort_by(|a, b| a.id.cmp(&b.id));
        let mut expected: Vec<Item> = one_at_a_time.iter().collect();
        expected.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(taken, expected);
    }

    #[test]
    fn a_site_is_held_as_its_stored_line() {
        let sites = upserted(&incoming());
        for site in sites.iter() {
            let held = sites.held.get(site.id.as_str()).unwrap();
            assert_eq!(held.domain(), site.id);
            assert_eq!(held.line(), stored::item_as_line(&site).unwrap());
            assert_eq!(held.score(), site::link_score(&site));
        }
        // A site known only by its ranks takes a line of under 100 bytes,
        // and is held in one allocation of the line, its domain and 8
        // bytes more.
        let ranks = Signals {
            tranco_rank: Some(1_234),
            harmonic_rank: Some(56_789),
            ..Signals::default()
        };
        let ranked = ranked("example.com", ranks);
        let line = stored::item_as_line(&ranked).unwrap();
        assert!(line.len() < 100, "{}", String::from_utf8_lossy(&line));
        let held = Held::new("example.com", ranked);
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
        let sites = upserted(&incoming());
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

    /// A list changed a site at a time holds what the same changes to its
    /// items make, each as its line, and moves the lines it does not change.
    #[test]
    fn a_list_changes_as_its_items_do() {
        let sites = upserted(&incoming());
        let items: Vec<Item> = sites.into_sorted().into_iter().collect();
        assert!(items.len() >= 4);
        let lines = |items: &[Item]| -> Vec<Vec<u8>> {
            items
                .iter()
                .map(|site| stored::item_as_line(site).unwrap())
                .collect()
        };
        let mut list = SiteList::new();
        for site in &items {
            list.push(site.clone());
        }
        assert_eq!(list, items.iter().cloned().collect::<SiteList>());
        assert_eq!(
            list.domains().collect::<Vec<_>>(),
            items.iter().map(|s| s.id.as_str()).collect::<Vec<_>>()
        );

        let crawled = |site: &Item| site::crawled(site).is_some();
        let (with, without) = list.clone().partition(crawled);
        let (want, rest): (Vec<Item>, Vec<Item>) = items.iter().cloned().partition(crawled);
        assert_eq!(
            with.lines().map(<[u8]>::to_vec).collect::<Vec<_>>(),
            lines(&want)
        );
        assert_eq!(
            without.lines().map(<[u8]>::to_vec).collect::<Vec<_>>(),
            lines(&rest)
        );

        let mut kept = list.clone();
        kept.retain(|site| site.title.is_some());
        let want: Vec<Item> = items
            .iter()
            .filter(|s| s.title.is_some())
            .cloned()
            .collect();
        assert_eq!(kept.iter().collect::<Vec<_>>(), want);
        let mut by_domain = list.clone();
        by_domain.retain_domains(|domain| domain != items[0].id);
        assert_eq!(by_domain.iter().collect::<Vec<_>>(), items[1..]);

        let mut changed = list.clone();
        changed.retain_mut(|site| {
            site.title = None;
            site.id != items[1].id
        });
        let mut want: Vec<Item> = items.clone();
        want.remove(1);
        for site in &mut want {
            site.title = None;
        }
        assert_eq!(
            changed.lines().map(<[u8]>::to_vec).collect::<Vec<_>>(),
            lines(&want)
        );

        let mut sorted = list.clone();
        sorted.sort_by_cached_key(|site| Reverse(site.id.clone()));
        let mut want = items.clone();
        want.sort_by_key(|site| Reverse(site.id.clone()));
        assert_eq!(sorted.iter().collect::<Vec<_>>(), want);

        let mut head = list.clone();
        let tail = head.split_off(1);
        assert_eq!((head.len(), tail.len()), (1, items.len() - 1));
        let mut joined = head.clone();
        joined.extend(tail.into_lines());
        assert_eq!(joined, list);
        joined.extend(vec![items[0].clone()]);
        assert_eq!(joined.len(), items.len() + 1);

        let mut one = list.into_lines().next().unwrap();
        assert_eq!(one.domain(), items[0].id);
        assert_eq!(one.item(), items[0]);
        assert_eq!(one.line(), stored::item_as_line(&items[0]).unwrap());
        assert_eq!(one, SiteLine::of(items[0].clone()));
        let json = serde_json::to_string(&one).unwrap();
        assert_eq!(json.as_bytes(), one.line());
        assert_eq!(serde_json::from_str::<SiteLine>(&json).unwrap(), one);
        let had = one.update(|site| site.title.take());
        assert_eq!(had, items[0].title);
        assert_eq!(one.item().title, None);
        assert_eq!(one.domain(), items[0].id);
    }

    #[test]
    fn sites_are_kept_by_domain_without_reading_them() {
        let mut sites = upserted(&incoming());
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
