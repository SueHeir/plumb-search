//! The denylist: signed entries that take a page, a site, an item or a
//! publisher off a node, applied before anything is indexed, shown or
//! passed on.
//!
//! A denylist is a set like any other whose only kind is `deny`
//! ([`crate::Manifest::is_denylist`]), and each entry is one of its items:
//!
//! ```json
//! {"id": "2026-10-12-0001", "kind": "deny", "target": "url-sha256:9f2c...",
//!  "category": "csam", "where": ["*"], "added": "2026-10-12"}
//! ```
//!
//! | Target | Takes off |
//! |---|---|
//! | `url:<canonical URL>` | one page |
//! | `url-sha256:<hex>` | one page, by the SHA-256 of its match key, so the list never spells out the address |
//! | `prefix:<canonical URL>` | that page and every page under its path |
//! | `site:<domain>` | every page on that domain or a name under it |
//! | `item:<publisher>/<set>/<id>` | one item of one set |
//! | `publisher:<key>` | everything that key publishes or signs |
//!
//! Pages are compared by their match key ([`crate::urls`]), so http and
//! https, `www.` and tracking parameters don't get past an entry. A node
//! looks up two keys for every URL, its match key and its match key
//! without the query: an entry for a page without a query covers it with
//! any query, so `?x=1` doesn't get past it, while an entry with a query
//! covers only that query. A `site:` entry covers every name under the
//! domain whatever the suffix lists say, so what an entry takes off never
//! changes with data sent later.
//!
//! `category` says why (`csam`, `copyright`, `privacy`, `court-order`,
//! `terror`, `malware`, `spam`; new ones need no change). A node's owner
//! can turn the project's list off, but its `csam` entries stay on
//! ([`Switch::Off`]). Nobody opens a csam report, so a csam entry names one
//! page, by its hash, and never a site or a prefix.
//!
//! `where` lists the jurisdictions an entry is for (`*`, `US`, `DE`); it is
//! read and kept, but every entry applies everywhere for now. Undoing an
//! entry is a tombstone on it, like any other item.
//!
//! A node applies every entry it understands. An entry with a newer target,
//! or made with a later URL rule than `url/1` (named in its `canonical`
//! key), is passed over, never the whole list, so a list cannot fail open
//! by growing.

use std::collections::HashMap;
use std::fmt;

use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

use crate::item::Item;
use crate::manifest::Manifest;
use crate::refs::{is_domain, is_namespace, Ref};
use crate::sign::PublicKey;
use crate::urls::{is_canonical, match_key, URL_VERSION};
use crate::value::{hex, is_date};

/// The kind of every denylist entry, and the only kind of a denylist.
pub const DENY: &str = "deny";

/// The category whose entries stay on when a node's owner turns a list
/// off.
pub const CSAM: &str = "csam";

/// What an entry takes off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// One page, by its canonical URL.
    Url(String),
    /// One page, by the SHA-256 of its match key.
    UrlSha256([u8; 32]),
    /// A canonical URL without a query: that page and every page under its
    /// path.
    Prefix(String),
    /// A domain and every name under it.
    Site(String),
    /// One item of one set.
    Item {
        publisher: PublicKey,
        set: String,
        id: String,
    },
    /// Everything a key publishes or signs.
    Publisher(PublicKey),
}

impl Target {
    /// Reads a target. `Ok(None)` for a scheme this version does not know;
    /// an error for a known scheme with a bad value.
    pub fn parse(text: &str) -> Result<Option<Target>> {
        let (scheme, value) = text
            .split_once(':')
            .with_context(|| format!("target {text:?} has no scheme"))?;
        Ok(Some(match scheme {
            "url" => {
                ensure!(is_canonical(value), "{value:?} is not a canonical URL");
                Target::Url(value.to_string())
            }
            "url-sha256" => Target::UrlSha256(
                hex32(value)
                    .with_context(|| format!("{value:?} is not 64 lower-case hex digits"))?,
            ),
            "prefix" => {
                ensure!(
                    is_canonical(value) && Url::parse(value).is_ok_and(|u| u.query().is_none()),
                    "{value:?} is not a canonical URL without a query"
                );
                Target::Prefix(value.to_string())
            }
            "site" => {
                ensure!(is_domain(value), "{value:?} is not a domain");
                Target::Site(value.to_string())
            }
            "item" => {
                let Ref::Item { publisher, set, id } = text.parse()? else {
                    unreachable!("an item: ref parses as Ref::Item")
                };
                Target::Item {
                    publisher: publisher.parse()?,
                    set,
                    id,
                }
            }
            "publisher" => Target::Publisher(value.parse()?),
            "http" | "https" => bail!("target {text:?} needs url: or prefix: before the address"),
            _ => return Ok(None),
        }))
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Url(url) => write!(f, "url:{url}"),
            Target::UrlSha256(hash) => write!(f, "url-sha256:{}", hex(hash)),
            Target::Prefix(url) => write!(f, "prefix:{url}"),
            Target::Site(domain) => write!(f, "site:{domain}"),
            Target::Item { publisher, set, id } => write!(f, "{}", item_ref(publisher, set, id)),
            Target::Publisher(key) => write!(f, "publisher:{key}"),
        }
    }
}

/// One denylist entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    pub target: Target,
    pub category: String,
    /// `where`: the jurisdictions it is for, `*` for all.
    pub places: Vec<String>,
    /// When it was added, as a date ([`crate::value::is_date`]).
    pub added: String,
}

impl Entry {
    /// Reads a denylist's item as an entry. `Ok(None)` for one this version
    /// does not understand: a newer target, or hashed with a newer URL rule
    /// than `url/1`. An error for one that breaks the rules, a tombstone
    /// among them.
    pub fn from_item(item: &Item) -> Result<Option<Entry>> {
        let id = &item.id;
        ensure!(
            item.kind == DENY,
            "item {id:?} is a {:?}, not a denylist entry",
            item.kind
        );
        ensure!(!item.is_deleted(), "entry {id:?} is a tombstone");
        ensure!(
            item.extends.is_none() && item.url.is_none() && item.site.is_none(),
            "entry {id:?} names a page or site outside its target"
        );
        let text = |key: &str| -> Result<&str> {
            item.other
                .get(key)
                .with_context(|| format!("entry {id:?} has no {key}"))?
                .as_str()
                .with_context(|| format!("entry {id:?}: {key} is not text"))
        };
        let category = text("category")?;
        ensure!(
            is_namespace(category),
            "entry {id:?}: {category:?} is not a category"
        );
        let places = match item.other.get("where") {
            None => vec!["*".to_string()],
            Some(Value::Array(places)) => places
                .iter()
                .map(|place| match place.as_str() {
                    Some(place) if is_place(place) => Ok(place.to_string()),
                    _ => bail!("entry {id:?}: {place} is not * or a jurisdiction"),
                })
                .collect::<Result<_>>()?,
            Some(other) => bail!("entry {id:?}: where {other} is not a list"),
        };
        ensure!(!places.is_empty(), "entry {id:?} applies nowhere");
        let added = text("added")?;
        ensure!(
            is_date(added),
            "entry {id:?}: added {added:?} is not a date"
        );
        if item.other.contains_key("canonical") && text("canonical")? != URL_VERSION {
            return Ok(None);
        }
        let target = text("target")?;
        let Some(target) = Target::parse(target).with_context(|| format!("entry {id:?}"))? else {
            return Ok(None);
        };
        if category == CSAM {
            ensure!(
                matches!(target, Target::UrlSha256(_)),
                "entry {id:?} names {target}: a csam entry names one page, by its hash"
            );
        }
        Ok(Some(Entry {
            id: id.clone(),
            target,
            category: category.to_string(),
            places,
            added: added.to_string(),
        }))
    }

    /// True for an entry that stays on when its list is turned off.
    pub fn is_locked(&self) -> bool {
        self.category == CSAM
    }
}

/// Whether a node's owner has a list on, or turned it off. A list that is
/// off still applies its `csam` entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
    On,
    Off,
}

/// The entry that took something off: its id and category, for the node's
/// log and its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Denial<'a> {
    pub id: &'a str,
    pub category: &'a str,
}

impl Denial<'_> {
    /// True for a `csam` entry.
    pub fn is_locked(&self) -> bool {
        self.category == CSAM
    }
}

/// One denylist, ready to look things up in. Built again when the list
/// changes.
#[derive(Debug)]
pub struct Denylist {
    switch: Switch,
    /// Each entry's id and the place of its category in `categories`.
    entries: Vec<(Box<str>, u16)>,
    categories: Vec<String>,
    /// By the SHA-256 of a match key: `url:` and `url-sha256:` entries.
    pages: HashMap<[u8; 32], u32>,
    /// By the match key of the prefix.
    prefixes: HashMap<Box<str>, u32>,
    /// By domain, without a leading `www.`.
    sites: HashMap<Box<str>, u32>,
    /// By `item:` ref.
    items: HashMap<Box<str>, u32>,
    publishers: HashMap<PublicKey, u32>,
}

impl Denylist {
    pub fn new(switch: Switch) -> Denylist {
        Denylist {
            switch,
            entries: Vec::new(),
            categories: Vec::new(),
            pages: HashMap::new(),
            prefixes: HashMap::new(),
            sites: HashMap::new(),
            items: HashMap::new(),
            publishers: HashMap::new(),
        }
    }

    /// Adds `entry`, unless the list is off and the entry is not locked.
    /// Returns whether it was added.
    pub fn add(&mut self, entry: Entry) -> bool {
        if self.switch == Switch::Off && !entry.is_locked() {
            return false;
        }
        let category = match self.categories.iter().position(|c| *c == entry.category) {
            Some(at) => at,
            None => {
                self.categories.push(entry.category);
                self.categories.len() - 1
            }
        };
        let at = u32::try_from(self.entries.len()).expect("fewer than 4 billion entries");
        self.entries.push((
            entry.id.into(),
            u16::try_from(category).expect("fewer than 65,536 categories"),
        ));
        let (entries, categories) = (&self.entries, &self.categories);
        let locked = |at: u32| categories[usize::from(entries[at as usize].1)] == CSAM;
        match entry.target {
            Target::Url(url) => {
                let key = match_key(&url).expect("a url: target is a canonical URL");
                file(&mut self.pages, sha256(&key), at, locked);
            }
            Target::UrlSha256(hash) => file(&mut self.pages, hash, at, locked),
            Target::Prefix(url) => {
                let key = match_key(&url).expect("a prefix: target is a canonical URL");
                file(&mut self.prefixes, key.into(), at, locked);
            }
            Target::Site(domain) => file(&mut self.sites, without_www(&domain).into(), at, locked),
            Target::Item { publisher, set, id } => {
                let key = item_ref(&publisher, &set, &id).to_string();
                file(&mut self.items, key.into(), at, locked);
            }
            Target::Publisher(key) => file(&mut self.publishers, key, at, locked),
        }
        true
    }

    /// Reads and adds every entry of a denylist's items, passing over
    /// tombstones. Returns how many it added and, for each item it could
    /// not use, why: an entry this version does not understand, or one that
    /// breaks the rules. Neither stops the rest from applying.
    pub fn add_items<'a>(
        &mut self,
        items: impl IntoIterator<Item = &'a Item>,
    ) -> (usize, Vec<String>) {
        let (mut added, mut passed_over) = (0, Vec::new());
        for item in items {
            if item.is_deleted() {
                continue;
            }
            match Entry::from_item(item) {
                Ok(Some(entry)) => added += usize::from(self.add(entry)),
                Ok(None) => passed_over.push(format!(
                    "entry {:?} is newer than this version understands",
                    item.id
                )),
                Err(error) => passed_over.push(format!("{error:#}")),
            }
        }
        (added, passed_over)
    }

    /// How many entries it applies.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry that takes `url` off, if one does. A `csam` entry is named
    /// before any other.
    pub fn denies_url(&self, url: &str) -> Option<Denial<'_>> {
        let mut found = None;
        self.look_up_url(url, &mut found);
        found.map(|at| self.denial(at))
    }

    /// The entry that takes `item` off, if one does: by its URL, its site,
    /// the item it extends, or, when it comes from a set (`from`), by its
    /// set's publisher or signer or by its own `item:` ref. A `csam` entry
    /// is named before any other.
    ///
    /// What an item is *about* (an `about` edge) is not looked up, so the
    /// Wikipedia article about a denied site stays.
    pub fn denies(&self, item: &Item, from: Option<&Manifest>) -> Option<Denial<'_>> {
        let mut found = None;
        if let Some(manifest) = from {
            let signers = std::iter::once(&manifest.publisher).chain(&manifest.signed_by);
            for key in signers {
                self.consider(&mut found, self.publishers.get(key));
            }
            if !self.items.is_empty() {
                let key = item_ref(&manifest.publisher, &manifest.set, &item.id).to_string();
                self.consider(&mut found, self.items.get(key.as_str()));
            }
        }
        if let Some(url) = &item.url {
            self.look_up_url(url, &mut found);
        }
        if let Some(site) = &item.site {
            self.look_up_site(site, &mut found);
        }
        match &item.extends {
            Some(Ref::Url(url)) => self.look_up_url(url, &mut found),
            Some(Ref::Site(site)) => self.look_up_site(site, &mut found),
            Some(target @ Ref::Item { publisher, .. }) => {
                if let Ok(publisher) = publisher.parse::<PublicKey>() {
                    self.consider(&mut found, self.publishers.get(&publisher));
                }
                if !self.items.is_empty() {
                    self.consider(&mut found, self.items.get(target.to_string().as_str()));
                }
            }
            _ => {}
        }
        found.map(|at| self.denial(at))
    }

    fn look_up_url(&self, url: &str, found: &mut Option<u32>) {
        if self.pages.is_empty() && self.prefixes.is_empty() && self.sites.is_empty() {
            return;
        }
        let Some(key) = match_key(url) else {
            return;
        };
        let bare = key.split_once('?').map_or(key.as_str(), |(bare, _)| bare);
        if !self.pages.is_empty() {
            self.consider(found, self.pages.get(&sha256(&key)));
            if bare != key {
                self.consider(found, self.pages.get(&sha256(bare)));
            }
        }
        // A match key is the host, with a port when it has one, then the
        // path, which always starts with `/`.
        let slash = bare.find('/').unwrap_or(bare.len());
        let (host, mut path) = bare.split_at(slash);
        if !self.prefixes.is_empty() {
            let mut probe = String::with_capacity(bare.len());
            loop {
                probe.clear();
                probe.push_str(host);
                probe.push_str(path);
                self.consider(found, self.prefixes.get(probe.as_str()));
                path = match path.rfind('/') {
                    Some(0) if path.len() > 1 => "/",
                    Some(at) if at > 0 => &path[..at],
                    _ => break,
                };
            }
        }
        self.look_up_site(without_port(host), found);
    }

    /// Looks up `host` and every name it is under, down to two labels. An
    /// IP address is looked up only as it is.
    fn look_up_site(&self, host: &str, found: &mut Option<u32>) {
        if self.sites.is_empty() {
            return;
        }
        let mut name = without_www(host);
        if name.parse::<std::net::Ipv4Addr>().is_ok() || name.starts_with('[') {
            self.consider(found, self.sites.get(name));
            return;
        }
        loop {
            self.consider(found, self.sites.get(name));
            match name.split_once('.') {
                Some((_, rest)) if rest.contains('.') => name = rest,
                _ => break,
            }
        }
    }

    fn denial(&self, at: u32) -> Denial<'_> {
        let (id, category) = &self.entries[at as usize];
        Denial {
            id,
            category: &self.categories[usize::from(*category)],
        }
    }

    fn is_locked(&self, at: u32) -> bool {
        self.denial(at).is_locked()
    }

    /// Keeps the first entry found, unless a locked one turns up after it.
    fn consider(&self, found: &mut Option<u32>, hit: Option<&u32>) {
        if let Some(&at) = hit {
            match *found {
                Some(was) if self.is_locked(was) || !self.is_locked(at) => {}
                _ => *found = Some(at),
            }
        }
    }
}

/// Files entry `at` under `key`, keeping a locked entry over one that is
/// not, and otherwise the first.
fn file<K: std::hash::Hash + Eq>(
    map: &mut HashMap<K, u32>,
    key: K,
    at: u32,
    locked: impl Fn(u32) -> bool,
) {
    let was = map.entry(key).or_insert(at);
    if !locked(*was) && locked(at) {
        *was = at;
    }
}

fn item_ref(publisher: &PublicKey, set: &str, id: &str) -> Ref {
    Ref::Item {
        publisher: publisher.to_string(),
        set: set.to_string(),
        id: id.to_string(),
    }
}

fn without_www(host: &str) -> &str {
    host.strip_prefix("www.")
        .filter(|rest| rest.contains('.'))
        .unwrap_or(host)
}

/// The host part of a match key without its port: `[::1]:8080` is `[::1]`.
fn without_port(host: &str) -> &str {
    match host.rfind(':') {
        Some(at) if !host[at..].contains(']') => &host[..at],
        _ => host,
    }
}

/// `*`, or a two-letter jurisdiction: an ISO 3166 country or `EU`.
fn is_place(text: &str) -> bool {
    text == "*" || (text.len() == 2 && text.bytes().all(|b| b.is_ascii_uppercase()))
}

fn sha256(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

fn hex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(text.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::urls::{url_sha256, url_sha256_any_query};
    use ed25519_dalek::SigningKey;
    use serde_json::json;

    fn key(seed: u8) -> PublicKey {
        PublicKey::of(&SigningKey::from_bytes(&[seed; 32]))
    }

    fn entry(id: &str, target: &str, category: &str) -> Item {
        serde_json::from_value(json!({
            "id": id, "kind": "deny", "target": target, "category": category,
            "where": ["*"], "added": "2026-10-12"
        }))
        .unwrap()
    }

    fn list(entries: &[Item], switch: Switch) -> Denylist {
        let mut list = Denylist::new(switch);
        let (_, passed_over) = list.add_items(entries);
        assert!(passed_over.is_empty(), "{passed_over:?}");
        list
    }

    fn page(url: &str) -> Item {
        serde_json::from_value(json!({"id": url, "kind": "page", "url": url})).unwrap()
    }

    fn set(publisher: PublicKey, signed_by: Option<PublicKey>, name: &str) -> Manifest {
        serde_json::from_value(json!({
            "format": "plumb-set/1", "set": name, "publisher": publisher.to_string(),
            "signed_by": signed_by.map(|k| k.to_string()),
            "generation": "2026-10-12T06:00:00Z",
            "files": [{"path": "items-001.jsonl.zst", "count": 1, "bytes": 1,
                       "sha256": "0".repeat(64)}]
        }))
        .unwrap()
    }

    #[test]
    fn targets_read_and_write() {
        let hash = "9f2c3b0e7d6a5f4e3d2c1b0a9f8e7d6c5b4a39281706f5e4d3c2b1a098f7e6d5";
        let publisher = format!("publisher:{}", key(1));
        let item = format!("item:{}/plumb%2Fdocs/a%2Fb", key(1));
        for text in [
            "url:https://example.org/a?b=1",
            &format!("url-sha256:{hash}"),
            "prefix:https://example.org/files/",
            "site:example.org",
            &item,
            &publisher,
        ] {
            let target = Target::parse(text).unwrap().unwrap();
            assert_eq!(target.to_string(), text);
        }
        assert_eq!(Target::parse("blob:sha256-00").unwrap(), None);
        for bad in [
            "https://example.org/",
            "url:https://Example.org/",
            "url-sha256:ABC",
            "prefix:https://example.org/?q=1",
            "site:Example.org",
            "item:k/plumb%2Fdocs/1",
            "publisher:k",
        ] {
            assert!(Target::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn entries_keep_the_rules() {
        let hash = url_sha256("https://example.org/a").unwrap();
        let read = Entry::from_item(&entry("1", &hash, "csam"))
            .unwrap()
            .unwrap();
        assert!(read.is_locked());
        assert_eq!(read.places, ["*"]);

        // Nobody opens a csam report, so its entry names one page by hash.
        for target in [
            "url:https://example.org/a",
            "site:example.org",
            "prefix:https://example.org/",
        ] {
            assert!(
                Entry::from_item(&entry("2", target, "csam")).is_err(),
                "{target}"
            );
        }

        let mut no_added = entry("3", "site:example.org", "spam");
        no_added.other.remove("added");
        let mut nowhere = entry("4", "site:example.org", "spam");
        nowhere.other.insert("where".into(), json!([]));
        let mut lower_place = entry("5", "site:example.org", "spam");
        lower_place.other.insert("where".into(), json!(["us"]));
        let mut with_url = entry("6", "site:example.org", "spam");
        with_url.url = Some("https://example.org/".into());
        let mut gone = entry("7", "site:example.org", "spam");
        gone.deleted = Some(crate::Deleted {
            at: "2026-10-13".into(),
            ..Default::default()
        });
        for bad in [
            no_added,
            nowhere,
            lower_place,
            with_url,
            gone,
            entry("8", "site:example.org", "Spam"),
            entry("9", "site:example.org", ""),
            page("https://example.org/"),
        ] {
            assert!(Entry::from_item(&bad).is_err(), "{bad:?}");
        }

        // Newer than this version: passed over, not refused.
        let mut url_2 = entry("10", "url:https://example.org/", "spam");
        url_2.other.insert("canonical".into(), json!("url/2"));
        assert_eq!(Entry::from_item(&url_2).unwrap(), None);
        assert_eq!(
            Entry::from_item(&entry("11", "blob:sha256-00", "spam")).unwrap(),
            None
        );
    }

    #[test]
    fn pages_by_url_and_by_hash() {
        let hash = url_sha256("https://example.org/report?id=7").unwrap();
        let any_query = url_sha256_any_query("https://example.org/watch?v=abc").unwrap();
        let list = list(
            &[
                entry("page", "url:https://example.org/page", "copyright"),
                entry("query", &hash, "privacy"),
                entry("any", &any_query, "csam"),
            ],
            Switch::On,
        );
        for url in [
            "https://example.org/page",
            "http://www.example.org/page/",
            "https://example.org/page?utm_source=x",
            "https://example.org/page?x=1",
        ] {
            assert_eq!(list.denies_url(url).unwrap().id, "page", "{url}");
        }
        assert_eq!(
            list.denies_url("http://www.example.org/report?utm_medium=y&id=7")
                .unwrap()
                .id,
            "query"
        );
        for url in [
            "https://example.org/watch?v=abc",
            "https://example.org/watch",
            "https://example.org/watch?v=other&x=1",
        ] {
            assert_eq!(list.denies_url(url).unwrap().id, "any", "{url}");
        }
        for url in [
            "https://example.org/pages",
            "https://example.org/report?id=8",
            "https://example.org/report",
            "https://other.example.org/page",
            "https://example.org/",
            "magnet:?xt=urn:btih:abc",
        ] {
            assert_eq!(list.denies_url(url), None, "{url}");
        }
    }

    #[test]
    fn prefixes_cover_whole_path_segments() {
        let list = list(
            &[
                entry("files", "prefix:https://example.org/files/", "malware"),
                entry("root", "prefix:https://shop.example.net/", "spam"),
            ],
            Switch::On,
        );
        for url in [
            "https://example.org/files",
            "https://example.org/files/",
            "http://www.example.org/files/a/b.zip?x=1",
            "https://shop.example.net/",
            "https://shop.example.net/any/thing",
        ] {
            assert!(list.denies_url(url).is_some(), "{url}");
        }
        for url in [
            "https://example.org/filesystem",
            "https://example.org/",
            "https://example.org:8080/files/a",
            "https://cdn.example.org/files/a",
            "https://example.net/",
            "https://shop.example.net.evil.com/",
        ] {
            assert_eq!(list.denies_url(url), None, "{url}");
        }
    }

    #[test]
    fn sites_cover_every_name_under_them() {
        let list = list(
            &[
                entry("site", "site:example.org", "court-order"),
                entry("ip", "site:192.0.2.10", "malware"),
            ],
            Switch::On,
        );
        for url in [
            "https://example.org/",
            "http://www.example.org/a",
            "https://a.b.example.org:8443/c?d=1",
            "http://192.0.2.10/x",
        ] {
            assert!(list.denies_url(url).is_some(), "{url}");
        }
        for url in [
            "https://example.org.evil.com/",
            "https://notexample.org/",
            "https://example.com/",
            "http://10.192.0.2.10.example.net/",
        ] {
            assert_eq!(list.denies_url(url), None, "{url}");
        }

        // A site's own card and the layers on it go; the article about it
        // stays.
        let card: Item = serde_json::from_value(json!({
            "id": "example.org", "kind": "site", "site": "example.org"
        }))
        .unwrap();
        let layer: Item = serde_json::from_value(json!({
            "id": "example.org", "extends": "site:example.org", "labels": ["adult"]
        }))
        .unwrap();
        let article: Item = serde_json::from_value(json!({
            "id": "https://en.wikipedia.org/wiki/Example", "kind": "article",
            "url": "https://en.wikipedia.org/wiki/Example",
            "edges": [{"rel": "about", "to": "site:example.org"}]
        }))
        .unwrap();
        assert!(list.denies(&card, None).is_some());
        assert!(list.denies(&layer, None).is_some());
        assert_eq!(list.denies(&article, None), None);
    }

    #[test]
    fn items_and_publishers() {
        let (project, builder, other) = (key(1), key(2), key(3));
        let list = list(
            &[
                entry("one", &format!("item:{project}/plumb%2Fdocs/42"), "privacy"),
                entry("builder", &format!("publisher:{builder}"), "spam"),
                entry("other", &format!("publisher:{other}"), "copyright"),
            ],
            Switch::On,
        );
        let docs = set(project, None, "plumb/docs");
        let music = set(project, None, "plumb/music");
        let delegated = set(project, Some(builder), "plumb/docs");
        let theirs = set(other, None, "them/stories");
        let item = |id: &str| -> Item {
            serde_json::from_value(json!({"id": id, "kind": "docs-page"})).unwrap()
        };

        assert_eq!(list.denies(&item("42"), Some(&docs)).unwrap().id, "one");
        assert_eq!(list.denies(&item("42"), Some(&music)), None);
        assert_eq!(list.denies(&item("43"), Some(&docs)), None);
        assert_eq!(list.denies(&item("42"), None), None);
        assert_eq!(
            list.denies(&item("7"), Some(&delegated)).unwrap().id,
            "builder"
        );
        assert_eq!(list.denies(&item("7"), Some(&theirs)).unwrap().id, "other");

        let on_the_item: Item = serde_json::from_value(json!({
            "id": "x", "extends": format!("item:{project}/plumb%2Fdocs/42"), "labels": ["x"]
        }))
        .unwrap();
        let on_theirs: Item = serde_json::from_value(json!({
            "id": "y", "extends": format!("item:{other}/them%2Fstories/1"), "labels": ["x"]
        }))
        .unwrap();
        assert_eq!(list.denies(&on_the_item, None).unwrap().id, "one");
        assert_eq!(list.denies(&on_theirs, None).unwrap().id, "other");
    }

    #[test]
    fn a_list_turned_off_keeps_its_csam_entries() {
        let url = "https://example.org/a";
        let entries = [
            entry("copyright", "site:example.org", "copyright"),
            entry("csam", &url_sha256(url).unwrap(), "csam"),
        ];
        let on = list(&entries, Switch::On);
        assert_eq!(on.len(), 2);
        // Both match; the csam entry is the one named, for the log.
        assert_eq!(on.denies_url(url).unwrap().id, "csam");
        assert_eq!(
            on.denies_url("https://example.org/b").unwrap().id,
            "copyright"
        );

        let off = list(&entries, Switch::Off);
        assert_eq!(off.len(), 1);
        assert!(off.denies_url(url).unwrap().is_locked());
        assert_eq!(off.denies_url("https://example.org/b"), None);
    }

    #[test]
    fn what_it_cannot_read_never_stops_the_rest() {
        let mut url_2 = entry("newer-rule", "url:https://example.org/", "spam");
        url_2.other.insert("canonical".into(), json!("url/2"));
        let gone: Item = serde_json::from_value(json!({
            "id": "undone", "deleted": {"at": "2026-10-13", "reason": "counter-notice"}
        }))
        .unwrap();
        let items = [
            entry("newer-target", "blob:sha256-00", "malware"),
            url_2,
            entry("broken", "site:Example.org", "spam"),
            gone,
            entry("good", "site:example.org", "spam"),
        ];
        let mut list = Denylist::new(Switch::On);
        let (added, passed_over) = list.add_items(&items);
        assert_eq!(added, 1);
        assert_eq!(passed_over.len(), 3, "{passed_over:?}");
        assert!(list.denies_url("https://example.org/").is_some());
    }
}
