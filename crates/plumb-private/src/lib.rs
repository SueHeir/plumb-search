//! Private search in the browser.
//!
//! A Plumb node's `/private` page loads this crate, compiled to
//! WebAssembly. When someone searches there, the query never leaves their
//! browser: it works out the query's keys and their buckets
//! ([`plumb_core::keys`]), pads them with random buckets, fetches those
//! buckets of sites from the node (`GET /api/buckets/{table}/{bucket}`),
//! keeps the sites that match the keys and ranks them itself ([`rank`]).
//! The node only learns [`BUCKETS_PER_SEARCH`] bucket numbers, each shared
//! by a few hundred keys. `docs/private-search.md` has the details and the
//! build steps.
//!
//! Everything but the page glue (`browser`, only built for `wasm32`) is
//! plain Rust, tested natively.

pub mod rank;

#[cfg(target_arch = "wasm32")]
mod browser;

pub use plumb_core::keys::BUCKETS_PER_SEARCH;
use plumb_core::keys::{matches, query_keys};
use plumb_core::SiteRecord;
pub use rank::{rank, Options, Ranked};
use sha2::{Digest, Sha256};

/// Results shown when the page does not ask for more.
pub const LIMIT: usize = 10;
/// Largest bucket answer accepted, in bytes.
pub const MAX_BUCKET_BYTES: usize = 16 << 20;

/// The sites of `answers` (one list of records per bucket fetched) that
/// match `keys`, each once, ranked for `query`.
pub fn search(
    query: &str,
    keys: &[String],
    answers: Vec<Vec<SiteRecord>>,
    options: &Options,
    limit: usize,
) -> Vec<Ranked> {
    let found: Vec<SiteRecord> = answers
        .into_iter()
        .flatten()
        .filter(|site| matches(site, keys))
        .collect();
    rank(query, &rank::dedupe(found), options, limit)
}

/// Reads one bucket answer: a JSON list of site records. Records that do
/// not read are skipped.
pub fn read_bucket(json: &str) -> Result<Vec<SiteRecord>, String> {
    if json.len() > MAX_BUCKET_BYTES {
        return Err("a bucket was too large".into());
    }
    let values: Vec<serde_json::Value> =
        serde_json::from_str(json).map_err(|_| "a bucket did not read".to_string())?;
    Ok(values
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
}

/// The random numbers that pick a search's padding buckets: the same for
/// the same query (its keys) in the same browser, whose `secret` stays in
/// its local storage, and unpredictable without that secret. So searching
/// for something again asks for the same buckets as before: the padding
/// cannot be told from the real buckets by comparing the two searches.
pub fn padding(secret: &[u8], query: &str) -> impl FnMut() -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"plumb-private-padding-v1\0");
    hasher.update(secret);
    hasher.update(b"\0");
    hasher.update(query_keys(query).join(" ").as_bytes());
    let seed: [u8; 32] = hasher.finalize().into();
    let mut counter: u64 = 0;
    move || {
        counter += 1;
        let hash = Sha256::new()
            .chain_update(seed)
            .chain_update(counter.to_le_bytes())
            .finalize();
        u64::from_le_bytes(hash[..8].try_into().expect("8 bytes"))
    }
}

/// The link to show for a result: only `http` and `https` URLs.
pub fn safe_href(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    matches!(parsed.scheme(), "http" | "https").then(|| parsed.to_string())
}

/// The two-letter country of a browser language tag: `en-US` -> `US`.
pub fn language_country(tag: &str) -> Option<String> {
    tag.split(['-', '_'])
        .skip(1)
        .find(|part| part.len() == 2 && part.chars().all(|c| c.is_ascii_alphabetic()))
        .and_then(plumb_core::normalize_country)
}

/// The query in a page fragment such as `#q=us%20bank`, decoded: `+` is a
/// space, as in a form's query string.
pub fn query_from_fragment(fragment: &str, decode: impl Fn(&str) -> Option<String>) -> String {
    fragment
        .trim_start_matches('#')
        .split('&')
        .find_map(|pair| pair.strip_prefix("q="))
        .and_then(|value| decode(&value.replace('+', " ")))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
