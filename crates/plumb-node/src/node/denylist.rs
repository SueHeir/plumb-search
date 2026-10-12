//! The project's denylist in a node ([`plumb_format::deny`]): the signed
//! `plumb/denylist` set the node takes like its other sets
//! ([`super::signed_sets`]), applied to what the node shows.
//!
//! The node reads the list it holds at start, and again whenever it takes
//! a newer one, so an entry takes a result off at once, without waiting
//! for an index to be built again. Every search result, page, key page,
//! headline and site it describes is looked up by its URL before it is
//! shown: in the node's own pages, and in what it answers through its API
//! and MCP tools.
//!
//! The owner can turn the list off ([`NodeConfig::project_denylist`]); its
//! `csam` entries apply anyway. A result a `csam` entry takes off is
//! logged by the entry's id, never by its address.
//!
//! Not yet: leaving denied pages out of the indexes as they are built, and
//! out of what the node passes on to other nodes.
//!
//! [`NodeConfig::project_denylist`]: super::NodeConfig::project_denylist

use std::path::Path;
use std::sync::{Arc, PoisonError};

use anyhow::Result;
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
/// owner's `switch`; `None` when the node holds none.
pub(super) fn read_held(
    data: &Path,
    publishers: &[PublicKey],
    switch: Switch,
) -> Result<Option<Denylist>> {
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
        return Ok(Some(list));
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
            Ok(Some(list)) => {
                *self
                    .denylist
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(list));
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
