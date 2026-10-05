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
//! hundred keys. The searcher then keeps the sites that [`matches`] its keys
//! and ranks them itself.
//!
//! Nodes searching each other (`plumb_net`) and browsers searching a site
//! privately (`plumb-private`) both use these, so they live here, with no
//! dependency that keeps them from compiling to WebAssembly.

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};

use crate::{domain_label, joined, normalize_text, SiteRecord};

/// Buckets keys are spread over.
pub const BUCKETS: u32 = 16_384;
/// Best sites kept per key, by link score.
pub const KEY_CAP: usize = 32;
/// Buckets fetched for every search, real ones padded with random ones.
pub const BUCKETS_PER_SEARCH: usize = 4;
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

/// Every key a site can be found by.
pub fn record_keys(record: &SiteRecord) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let label = domain_label(&record.domain);
    add_keys(&mut keys, &label);
    for part in label.split('-') {
        add_keys(&mut keys, part);
    }
    if let Some(title) = &record.title {
        add_keys(&mut keys, title);
        for part in title.split(TITLE_SEPARATORS).flat_map(|p| p.split(" - ")) {
            add_keys(&mut keys, part);
        }
    }
    for alias in &record.aliases {
        add_keys(&mut keys, alias);
    }
    let mut texts: Vec<_> = record.link_texts.iter().collect();
    texts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.text.cmp(&b.text)));
    for lt in texts.into_iter().take(KEY_LINK_TEXTS) {
        add_keys(&mut keys, &lt.text);
    }
    keys
}

/// The keys of a query, the whole query joined first, then its words,
/// longest first (they pick out the fewest sites).
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

/// Whether a site can be found by any of `keys`.
pub fn matches(record: &SiteRecord, keys: &[String]) -> bool {
    let mine = record_keys(record);
    keys.iter().any(|k| mine.contains(k))
}

/// `record` cut down to what finding and ranking it by its keys needs: the
/// [`KEY_LINK_TEXTS`] link texts whose keys count, without the sets of
/// sites that use them, and none of the crawler's bookkeeping. What a
/// browser downloads for a private search.
pub fn slim_record(mut record: SiteRecord) -> SiteRecord {
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
    record.icon = None;
    record.news = Vec::new();
    record.key_pages.clear();
    record.links_to.clear();
    record
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LinkText;

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
    fn padding_comes_from_the_random_source() {
        let mut n = 0u64;
        let (buckets, keys) = pick_buckets("x", || {
            n += 7919;
            n
        });
        assert!(keys.is_empty());
        assert_eq!(buckets.len(), BUCKETS_PER_SEARCH);
    }

    #[test]
    fn slim_records_keep_their_keys() {
        let mut r = SiteRecord::new("usbank.com");
        r.title = Some("U.S. Bank".into());
        r.crawl_failures = 3;
        r.crawled_at = Some(5);
        r.link_texts = (0..20)
            .map(|i| LinkText::from_linkers(format!("text {i}"), (1u64 << i) - 1))
            .collect();
        let slim = slim_record(r.clone());
        assert_eq!(record_keys(&slim), record_keys(&r));
        assert_eq!(slim.link_texts.len(), KEY_LINK_TEXTS);
        assert!(slim.link_texts.iter().all(|lt| lt.linkers == 0));
        assert_eq!(slim.link_texts[0].text, "text 19");
        assert_eq!((slim.crawl_failures, slim.crawled_at), (0, None));
    }
}
