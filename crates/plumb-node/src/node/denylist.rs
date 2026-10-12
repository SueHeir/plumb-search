//! The project's denylist in a node ([`plumb_format::deny`]): the signed
//! `plumb/denylist` set the node takes like its other sets
//! ([`super::signed_sets`]), applied to what the node shows, indexes and
//! passes on.
//!
//! The node reads the list it holds at start, and again whenever it takes
//! a newer one, so an entry takes a result off at once, without waiting
//! for an index to be built again. Every search result, page, key page,
//! headline and site it describes is looked up by its URL before it is
//! shown: in the node's own pages, and in what it answers through its API
//! and MCP tools.
//!
//! What the list denies is also left out of what the node keeps and
//! builds: the page index ([`super::pages`], built again when the list
//! changes), the search index and the buckets other nodes take
//! ([`super::worker`]), crawls and headlines other nodes send or fill
//! with, and its own feed checks. It passes none of it on: its own crawls
//! and headlines are published without it ([`Inner::drop_denied_records`]),
//! it keeps and lists no lead for a denied page, and it relays no stored
//! batch that names one, since a signed batch can't be passed on without
//! it ([`plumb_net::bucket::BucketSource::denies`]). Buckets take a new
//! entry at the next build of the search index.
//!
//! The owner can turn the list off ([`NodeConfig::project_denylist`]); its
//! `csam` entries apply anyway. A result a `csam` entry takes off is
//! logged by the entry's id, never by its address.
//!
//! [`NodeConfig::project_denylist`]: super::NodeConfig::project_denylist

use std::borrow::Cow;
use std::path::Path;
use std::sync::{Arc, PoisonError};

use anyhow::Result;
use plumb_core::SiteRecord;
use plumb_format::deny::{Denylist, Switch};
use plumb_format::PublicKey;
use plumb_index::pages::{Page, PageHit};
use plumb_index::{Hit, SearchResults};
use tracing::{info, warn};

use super::signed_sets::set_dir;
use super::Inner;
use crate::sets::{check_set, read_items};

/// The project's denylist set.
pub(super) const DENYLIST: &str = "plumb/denylist";

/// How many more sites a search ranks when the list may take some off.
pub(super) const MARGIN: usize = 10;

/// The denylist held in `data`, as one of `publishers` signed it, with the
/// owner's `switch`, and its stamp: its generation and the switch, which
/// change whenever what it denies does. `None` when the node holds none.
pub(super) fn read_held(
    data: &Path,
    publishers: &[PublicKey],
    switch: Switch,
) -> Result<Option<(Denylist, String)>> {
    let Some(dir) = set_dir(data, DENYLIST).filter(|dir| dir.join(crate::sets::MANIFEST).is_file())
    else {
        return Ok(None);
    };
    let mut failed = None;
    for publisher in publishers {
        if let Err(err) = check_set(&dir, Some(publisher), &[]) {
            failed = Some(err);
            continue;
        }
        let (manifest, items) = read_items(&dir)?;
        anyhow::ensure!(manifest.is_denylist(), "{} is not a denylist", manifest.set);
        let mut list = Denylist::new(switch);
        let (added, passed_over) = list.add_items(&items);
        if !passed_over.is_empty() {
            warn!(
                "the denylist has {} entries this version passes over; the first: {}",
                passed_over.len(),
                passed_over[0]
            );
        }
        info!(
            "applying the denylist of {} ({added} entries{})",
            manifest.generation,
            if switch == Switch::Off {
                ", only its csam ones: it is turned off"
            } else {
                ""
            }
        );
        let stamp = format!(
            "{}:{}",
            manifest.generation,
            if switch == Switch::Off { "off" } else { "on" }
        );
        return Ok(Some((list, stamp)));
    }
    match failed {
        Some(err) => Err(err.context("the denylist held is not one this node trusts")),
        None => Ok(None),
    }
}

impl Inner {
    /// The denylist in use, once the node holds one.
    pub(super) fn denylist(&self) -> Option<Arc<Denylist>> {
        self.denylist
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|(list, _)| list.clone())
    }

    /// The denylist in use with its stamp ([`read_held`]).
    pub(super) fn denylist_stamped(&self) -> Option<(Arc<Denylist>, String)> {
        self.denylist
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Reads the denylist the node holds and puts it in use. A list that
    /// can't be read leaves the one in use in place.
    pub(super) fn load_denylist(&self) {
        let switch = if self.config.project_denylist {
            Switch::On
        } else {
            Switch::Off
        };
        match read_held(&self.paths.data, &self.config.set_publishers, switch) {
            Ok(Some((list, stamp))) => {
                *self
                    .denylist
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = Some((Arc::new(list), stamp));
            }
            Ok(None) => {}
            Err(err) => warn!("{err:#}"),
        }
    }

    /// True when the denylist takes off the page at `url`.
    pub(super) fn denies(&self, url: &str) -> bool {
        self.denylist().is_some_and(|list| denies(&list, url))
    }

    /// Takes off what the denylist denies of `results`: sites, their key
    /// pages, the pages listed with them and the site search offered.
    pub(super) fn drop_denied(&self, results: &mut SearchResults) {
        let Some(list) = self.denylist() else {
            return;
        };
        drop_denied_hits(&list, &mut results.hits);
        results
            .pages
            .retain(|placed| !denies(&list, &placed.hit.page.url));
        if results
            .site_search
            .as_ref()
            .is_some_and(|search| denies(&list, &search.url))
        {
            results.site_search = None;
        }
    }

    /// `hits` without the pages the denylist denies.
    pub(super) fn drop_denied_pages(&self, mut hits: Vec<PageHit>) -> Vec<PageHit> {
        if let Some(list) = self.denylist() {
            hits.retain(|hit| !denies(&list, &hit.page.url));
        }
        hits
    }

    /// `page`, unless the denylist denies it.
    pub(super) fn unless_denied(&self, page: Option<Page>) -> Option<Page> {
        page.filter(|page| !self.denies(&page.url))
    }

    /// Takes off `records` the sites the denylist denies, and off the rest
    /// the key pages and headlines it denies ([`allowed_record`]), before
    /// they are kept or passed on. How many sites it took off.
    pub(super) fn drop_denied_records(&self, records: &mut Vec<SiteRecord>) -> usize {
        let Some(list) = self.denylist() else {
            return 0;
        };
        let before = records.len();
        records.retain_mut(|record| {
            let trimmed = match allowed_record(&list, record) {
                None => return false,
                Some(Cow::Borrowed(_)) => None,
                Some(Cow::Owned(trimmed)) => Some(trimmed),
            };
            if let Some(trimmed) = trimmed {
                *record = trimmed;
            }
            true
        });
        before - records.len()
    }

    /// `hit`, unless the denylist denies its site, without the key pages
    /// it denies.
    pub(super) fn unless_denied_site(&self, hit: Option<Hit>) -> Option<Hit> {
        let mut hits: Vec<Hit> = hit.into_iter().collect();
        if let Some(list) = self.denylist() {
            drop_denied_hits(&list, &mut hits);
        }
        hits.pop()
    }
}

/// `record` as the node may index or pass it on by `list`: `None` when
/// the list denies its site or homepage, else without the key pages and
/// headlines it denies (borrowed when it denies none).
pub(super) fn allowed_record<'a>(
    list: &Denylist,
    record: &'a SiteRecord,
) -> Option<Cow<'a, SiteRecord>> {
    let site = format!("https://{}/", record.domain);
    if denies(list, &site) || record.url.as_deref().is_some_and(|url| denies(list, url)) {
        return None;
    }
    let denied_page = |url: &str| denies(list, url);
    if !record.key_pages.iter().any(|page| denied_page(&page.url))
        && !record
            .news
            .iter()
            .any(|headline| denied_page(&headline.url))
    {
        return Some(Cow::Borrowed(record));
    }
    let mut trimmed = record.clone();
    trimmed.key_pages.retain(|page| !denied_page(&page.url));
    trimmed.news.retain(|headline| !denied_page(&headline.url));
    Some(Cow::Owned(trimmed))
}

/// Sites the list denies taken off `hits`, and denied key pages off the
/// rest.
fn drop_denied_hits(list: &Denylist, hits: &mut Vec<Hit>) {
    hits.retain(|hit| !denies(list, &hit.url));
    for hit in hits {
        hit.key_pages.retain(|page| !denies(list, &page.url));
    }
}

/// True when `list` takes off the page at `url`. A `csam` entry's hit is
/// logged, by the entry's id only.
fn denies(list: &Denylist, url: &str) -> bool {
    match list.denies_url(url) {
        Some(denial) => {
            if denial.is_locked() {
                warn!(
                    "denylist entry {} ({}) took a result off",
                    denial.id, denial.category
                );
            }
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use plumb_format::deny::Entry;
    use serde_json::json;

    use super::*;
    use crate::sets::{deny_entry, DenyEntryArgs};

    fn list(targets: &[&str]) -> Denylist {
        let mut list = Denylist::new(Switch::On);
        for target in targets {
            let entry = deny_entry(&DenyEntryArgs {
                category: "copyright".into(),
                target: Some(target.to_string()),
                hash_of: None,
                any_query: false,
                places: Vec::new(),
                id: None,
            })
            .unwrap();
            list.add(Entry::from_item(&entry).unwrap().unwrap());
        }
        list
    }

    #[test]
    fn a_record_is_kept_or_passed_on_without_what_the_list_denies() {
        let record: SiteRecord = serde_json::from_value(json!({
            "domain": "example.com", "url": "https://www.example.com/",
            "key_pages": [{"label": "Docs", "url": "https://example.com/docs"},
                          {"label": "Shop", "url": "https://example.com/shop"}],
            "news": [{"title": "Kept", "url": "https://example.com/news/1", "at": 1},
                     {"title": "Taken off", "url": "https://example.com/news/2", "at": 2}],
        }))
        .unwrap();
        let kept = |targets: &[&str]| allowed_record(&list(targets), &record).map(Cow::into_owned);

        assert!(matches!(
            allowed_record(&list(&["site:other.org"]), &record),
            Some(Cow::Borrowed(_))
        ));
        assert_eq!(kept(&["site:example.com"]), None, "a denied site");
        assert_eq!(
            kept(&["url:https://www.example.com/"]),
            None,
            "a denied homepage"
        );
        let trimmed = kept(&[
            "url:https://example.com/shop",
            "url:https://example.com/news/2",
        ])
        .unwrap();
        let pages: Vec<&str> = trimmed
            .key_pages
            .iter()
            .map(|page| page.url.as_str())
            .collect();
        assert_eq!(pages, ["https://example.com/docs"]);
        let headlines: Vec<&str> = trimmed.news.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(headlines, ["Kept"]);
        assert_eq!(
            SiteRecord {
                key_pages: record.key_pages.clone(),
                news: record.news.clone(),
                ..trimmed
            },
            record,
            "nothing else changes"
        );
    }
}
