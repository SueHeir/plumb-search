//! Bucket keys: how a search can ask for sites without saying what it is
//! looking for.
//!
//! Every name a site goes by (its domain label, homepage title, aliases and
//! top link texts) gives a few **keys**: each word, and each whole name with
//! the spaces taken out (`U.S. Bank` -> `us`, `bank`, `usbank`). Each key
//! falls in one of [`BUCKETS`] buckets by its hash. A node with an index
//! keeps, for every bucket, the best [`KEY_CAP`] sites of every key in it.
//!
//! A search works out the keys of its query and asks for their buckets,
//! never the query: [`BUCKETS_PER_SEARCH`] buckets every time, padded with
//! random ones. Whoever answers learns bucket numbers, each shared by a few
//! hundred keys. The searcher then keeps the sites that [`site_matches`]
//! its keys and ranks them itself.
//!
//! Nodes searching each other (`plumb_net`) and browsers searching a site
//! privately (`plumb-private`) both use these, so they live here, with no
//! dependency that keeps them from compiling to WebAssembly.
//!
//! A site's keys come from its stored item ([`crate::site`], [`site_keys`]):
//! a node's index build files its sites as items, and private search, PIR
//! tables and the tools cut items down ([`slim_site`], [`lean_site`]).

use std::collections::BTreeSet;

use plumb_format::Item;
use sha2::{Digest, Sha256};

use crate::{domain_label, is_function_word, joined, normalize_text, site, LinkText};

/// Buckets keys are spread over.
pub const BUCKETS: u32 = 16_384;
/// Best sites kept per key, by link score.
pub const KEY_CAP: usize = 32;
/// Buckets fetched for every search, real ones padded with random ones.
pub const BUCKETS_PER_SEARCH: usize = 4;
/// Pieces each bucket is cut into, by key, for private retrieval (PIR): a
/// PIR table packs pieces into rows of nearly equal size, where whole
/// buckets would differ several times over.
pub const PIECES_PER_BUCKET: u32 = 4;
/// Pieces in all.
pub const PIECES: u32 = BUCKETS * PIECES_PER_BUCKET;
/// Link texts, most used first, whose keys count.
pub const KEY_LINK_TEXTS: usize = 8;
/// Keys shorter than this are skipped: one- and two-letter words would put
/// a few huge buckets in every search.
const MIN_KEY_CHARS: usize = 2;
/// Characters that separate the parts of a homepage title, as in the index.
const TITLE_SEPARATORS: [char; 10] = ['|', '·', '•', ':', '–', '—', '»', '«', '/', '\\'];

/// The bucket of `key`. The same on every node, in every browser and in
/// every version of the protocol, which is why it has its own hash rather
/// than Rust's.
pub fn bucket_of(key: &str) -> u32 {
    let mut hasher = Sha256::new();
    hasher.update(b"plumb-bucket-v1\0");
    hasher.update(key.as_bytes());
    let hash: [u8; 32] = hasher.finalize().into();
    let prefix = u64::from_be_bytes(hash[..8].try_into().expect("8 bytes"));
    (prefix % u64::from(BUCKETS)) as u32
}

/// The piece of `key`: one of its bucket's [`PIECES_PER_BUCKET`], by a
/// second hash. Like [`bucket_of`], the same everywhere and in every
/// version.
pub fn piece_of(key: &str) -> u32 {
    let mut hasher = Sha256::new();
    hasher.update(b"plumb-piece-v1\0");
    hasher.update(key.as_bytes());
    let hash: [u8; 32] = hasher.finalize().into();
    let sub = u32::from_be_bytes(hash[..4].try_into().expect("4 bytes")) % PIECES_PER_BUCKET;
    bucket_of(key) * PIECES_PER_BUCKET + sub
}

/// The keys of the names in `text`: its words and the whole text joined.
fn add_keys(keys: &mut BTreeSet<String>, text: &str) {
    let normalized = normalize_text(text);
    for word in normalized.split(' ') {
        if word.chars().count() >= MIN_KEY_CHARS {
            keys.insert(word.to_string());
        }
    }
    let whole = joined(text);
    if whole.chars().count() >= MIN_KEY_CHARS {
        keys.insert(whole);
    }
}

/// Every key the site of a stored item ([`crate::site`]) can be found by.
pub fn site_keys(site: &Item) -> BTreeSet<String> {
    let link_texts = crate::site::link_texts(site);
    name_keys(&site.id, site.title.as_deref(), &site.names, &link_texts)
}

/// The keys of the names the site `domain` goes by: its domain label,
/// its homepage's `title` and the title's parts, its other `names`, and
/// its [`KEY_LINK_TEXTS`] most used `link_texts`.
fn name_keys(
    domain: &str,
    title: Option<&str>,
    names: &[String],
    link_texts: &[LinkText],
) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let label = domain_label(domain);
    add_keys(&mut keys, &label);
    for part in label.split('-') {
        add_keys(&mut keys, part);
    }
    if let Some(title) = title {
        add_keys(&mut keys, title);
        for part in title.split(TITLE_SEPARATORS).flat_map(|p| p.split(" - ")) {
            add_keys(&mut keys, part);
        }
    }
    for name in names {
        add_keys(&mut keys, name);
    }
    let mut texts: Vec<_> = link_texts.iter().collect();
    texts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.text.cmp(&b.text)));
    for lt in texts.into_iter().take(KEY_LINK_TEXTS) {
        add_keys(&mut keys, &lt.text);
    }
    keys
}

/// The keys of a query, the whole query joined first, then its words,
/// longest first (they pick out the fewest sites). In a query with other
/// words, [`crate::FUNCTION_WORDS`] are not keys: the bucket of "in" holds
/// sites such as "Social Security in Spain", which "weather in denver" is
/// not looking for. The whole query joined still has them
/// ("bankofamerica").
pub fn query_keys(query: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let whole = joined(query);
    if whole.chars().count() >= MIN_KEY_CHARS {
        keys.push(whole);
    }
    let normalized = normalize_text(query);
    let mut words: Vec<&str> = normalized
        .split(' ')
        .filter(|w| w.chars().count() >= MIN_KEY_CHARS)
        .collect();
    if words.iter().any(|w| !is_function_word(w)) {
        words.retain(|w| !is_function_word(w));
    }
    words.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    for word in words {
        if !keys.iter().any(|k| k == word) {
            keys.push(word.to_string());
        }
    }
    keys
}

/// The buckets to fetch for `query`: those of its first keys, and random
/// ones to make up [`BUCKETS_PER_SEARCH`], in random order. Also returns
/// the keys searched for. `random` gives random numbers; it must be a
/// cryptographic source, or the padding could be told from the real ones.
pub fn pick_buckets(query: &str, mut random: impl FnMut() -> u64) -> (Vec<u32>, Vec<String>) {
    let mut keys = query_keys(query);
    let mut buckets: Vec<u32> = Vec::new();
    let mut used = Vec::new();
    for key in keys.drain(..) {
        if buckets.len() == BUCKETS_PER_SEARCH {
            break;
        }
        let bucket = bucket_of(&key);
        if !buckets.contains(&bucket) {
            buckets.push(bucket);
        }
        used.push(key);
    }
    while buckets.len() < BUCKETS_PER_SEARCH {
        let bucket = (random() % u64::from(BUCKETS)) as u32;
        if !buckets.contains(&bucket) {
            buckets.push(bucket);
        }
    }
    for i in (1..buckets.len()).rev() {
        let j = (random() % (i as u64 + 1)) as usize;
        buckets.swap(i, j);
    }
    (buckets, used)
}

/// Whether the site of an item ([`crate::site`]) can be found by any of
/// `keys`, its [`site_keys`].
pub fn site_matches(site: &Item, keys: &[String]) -> bool {
    let mine = site_keys(site);
    keys.iter().any(|k| mine.contains(k))
}

/// Characters of a description a [`lean_site`] keeps.
pub const LEAN_DESCRIPTION_CHARS: usize = 200;

/// A site's item ([`crate::site`]) cut down to what finding and ranking it
/// by its keys needs: what a browser downloads for a private search. It
/// keeps the [`KEY_LINK_TEXTS`] link texts whose keys count, without the
/// sets of sites that use them, and none of the node's own fields (the
/// crawl time and marks, the icon and headlines) or edges (key pages,
/// sites linked to).
pub fn slim_site(mut site: Item) -> Item {
    let mut link_texts = site::link_texts(&site);
    link_texts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.text.cmp(&b.text)));
    link_texts.truncate(KEY_LINK_TEXTS);
    for lt in &mut link_texts {
        lt.linkers = 0;
    }
    site::set_link_texts(&mut site, &link_texts);
    site::set_crawled(&mut site, None);
    // The crawl marks whole, keys this version does not know among them.
    site.fields.remove("crawl");
    site::set_icon(&mut site, None);
    site::set_news(&mut site, Vec::new());
    site.edges.clear();
    site
}

/// A [`slim_site`] without the homepage's text, terms and headings and
/// the Wikipedia intro, and with the description cut to
/// [`LEAN_DESCRIPTION_CHARS`]: what a PIR table holds. On a node with 1.3M
/// sites it ranked name and description searches the same as full sites
/// (without vectors), at a third of the size.
pub fn lean_site(site: Item) -> Item {
    let mut site = slim_site(site);
    site.text = None;
    site::set_terms(&mut site, Vec::new());
    site::set_headings(&mut site, Vec::new());
    site::set_intro(&mut site, None);
    site.about = site
        .about
        .map(|about| about.chars().take(LEAN_DESCRIPTION_CHARS).collect());
    site
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SiteRecord;

    /// The keys of a site's record, as `record_keys` gave them before
    /// private search and the tools moved onto items: what [`site_keys`]
    /// gives of the site's item.
    fn record_keys(record: &SiteRecord) -> BTreeSet<String> {
        name_keys(
            &record.domain,
            record.title.as_deref(),
            &record.aliases,
            &record.link_texts,
        )
    }

    /// A record cut down as `slim_record` cut it then: what [`slim_site`]
    /// does to the record's item.
    fn slim_record(mut record: SiteRecord) -> SiteRecord {
        record
            .link_texts
            .sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.text.cmp(&b.text)));
        record.link_texts.truncate(KEY_LINK_TEXTS);
        for lt in &mut record.link_texts {
            lt.linkers = 0;
        }
        record.crawled_at = None;
        record.crawl_attempted_at = None;
        record.crawl_failures = 0;
        record.gone_at = None;
        record.icon = None;
        record.news = Vec::new();
        record.key_pages.clear();
        record.links_to.clear();
        record
    }

    /// A record cut down as `lean_record` cut it then: what [`lean_site`]
    /// does to the record's item.
    fn lean_record(record: SiteRecord) -> SiteRecord {
        let mut record = slim_record(record);
        record.body_text = None;
        record.terms.clear();
        record.headings.clear();
        record.intro = None;
        record.description = record
            .description
            .map(|d| d.chars().take(LEAN_DESCRIPTION_CHARS).collect());
        record
    }

    #[test]
    fn buckets_never_change() {
        // The protocol: changing these splits the network.
        assert_eq!(bucket_of("usbank"), bucket_of("usbank"));
        let known: Vec<u32> = ["usbank", "bank", "us", "chase"]
            .iter()
            .map(|k| bucket_of(k))
            .collect();
        assert!(known.iter().all(|&b| b < BUCKETS));
        assert_eq!(known, KNOWN_BUCKETS);
    }

    /// `bucket_of` of `usbank`, `bank`, `us` and `chase`, as first shipped.
    const KNOWN_BUCKETS: [u32; 4] = [5691, 2985, 15898, 14277];

    #[test]
    fn pieces_never_change_and_stay_in_their_bucket() {
        for key in ["usbank", "bank", "us", "chase"] {
            let piece = piece_of(key);
            assert!(piece < PIECES);
            assert_eq!(piece / PIECES_PER_BUCKET, bucket_of(key));
        }
        let known: Vec<u32> = ["usbank", "bank", "us", "chase"]
            .iter()
            .map(|k| piece_of(k) % PIECES_PER_BUCKET)
            .collect();
        assert_eq!(known, KNOWN_PIECES);
    }

    /// `piece_of(..) % PIECES_PER_BUCKET` of `usbank`, `bank`, `us` and
    /// `chase`, as first shipped.
    const KNOWN_PIECES: [u32; 4] = [1, 1, 3, 2];

    #[test]
    fn function_words_are_keys_only_on_their_own() {
        assert_eq!(
            query_keys("weather in denver"),
            vec!["weatherindenver", "weather", "denver"]
        );
        assert_eq!(
            query_keys("bank of america"),
            vec!["bankofamerica", "america", "bank"]
        );
        assert_eq!(query_keys("in the"), vec!["inthe", "the", "in"]);
        let mut spain = site::new("seg-social.es");
        spain.title = Some("Social Security in Spain".into());
        assert!(site_keys(&spain).contains("in"));
        assert!(!site_matches(&spain, &query_keys("weather in denver")));
    }

    #[test]
    fn a_site_s_item_has_its_record_s_keys() {
        let mut records = crate::site::tests::variants();
        records.push(crate::site_item::tests::full_record());
        let mut usbank = SiteRecord::new("us-bank.com");
        usbank.title = Some("U.S. Bank | Checking - Savings".into());
        usbank.link_texts = (0..20)
            .map(|i| LinkText::with_count(format!("text {i}"), i))
            .collect();
        records.push(usbank);
        for mut record in records {
            record.clean();
            let site = record.to_stored_item();
            assert_eq!(site_keys(&site), record_keys(&record), "{}", record.domain);
        }
    }

    #[test]
    fn padding_comes_from_the_random_source() {
        let mut n = 0u64;
        let (buckets, keys) = pick_buckets("x", || {
            n += 7919;
            n
        });
        assert!(keys.is_empty());
        assert_eq!(buckets.len(), BUCKETS_PER_SEARCH);
    }

    /// [`slim_site`] and [`lean_site`] cut a site's item down as
    /// [`slim_record`] and [`lean_record`] cut its record.
    #[test]
    fn a_site_s_item_is_cut_down_as_its_record() {
        let mut records = crate::site::tests::variants();
        records.push(crate::site_item::tests::full_record());
        let mut usbank = SiteRecord::new("usbank.com");
        usbank.description = Some("Checking and savings. ".repeat(20));
        usbank.link_texts = (0..20)
            .map(|i| LinkText::from_linkers(format!("text {i}"), (1u64 << (i % 5)) - 1))
            .collect();
        records.push(usbank);
        for record in records {
            let site = record.clone().stored_item_as_is();
            let slim = slim_record(record.clone()).stored_item_as_is();
            assert_eq!(slim_site(site.clone()), slim, "{}", record.domain);
            let lean = lean_record(record.clone()).stored_item_as_is();
            assert_eq!(lean_site(site), lean, "{}", record.domain);
        }
    }

    #[test]
    fn slim_sites_keep_their_keys() {
        let mut usbank = site::new("usbank.com");
        usbank.title = Some("U.S. Bank".into());
        let marks = site::CrawlMarks {
            failures: 3,
            ..Default::default()
        };
        site::set_crawl_marks(&mut usbank, marks);
        site::set_crawled(&mut usbank, Some(5));
        let link_texts: Vec<LinkText> = (0..20)
            .map(|i| LinkText::from_linkers(format!("text {i}"), (1u64 << i) - 1))
            .collect();
        site::set_link_texts(&mut usbank, &link_texts);
        let slim = slim_site(usbank.clone());
        assert_eq!(site_keys(&slim), site_keys(&usbank));
        let link_texts = site::link_texts(&slim);
        assert_eq!(link_texts.len(), KEY_LINK_TEXTS);
        assert!(link_texts.iter().all(|lt| lt.linkers == 0));
        assert_eq!(link_texts[0].text, "text 19");
        assert_eq!(site::crawl_marks(&slim), site::CrawlMarks::default());
        assert_eq!(site::crawled(&slim), None);
    }
}
