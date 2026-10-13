//! Inner pages a node goes looking for because its own searches wanted
//! them, so each node grows the pages its own people search for.
//!
//! A node holds sites' homepages, and the page sets bring a few inner
//! pages of hand-picked sites. When a search's best sites have no page
//! shown for the rest of its words ("who dengue fact sheet" shows who.int
//! but none of its fact sheets), the node notes those words for each of
//! them ([`WantedPages::note`]). In the background it follows each such
//! site's links toward pages with the words ([`plumb_crawl::follow_links`],
//! run by `node/wanted_pages.rs`), keeps the pages found, and searches
//! them next to the page sets as the [`WANTED_SET`] set, shown as a site's
//! inner pages are. Links that a page found makes to another site with
//! the words make that site wanted too, one hop out from the search's own
//! sites.
//!
//! The words stay on the node, and are forgotten after
//! [`WANT_KEPT`]; the sites see only the fetches, as a crawl's. Kept in
//! memory and on disk, at most [`MAX_SITES`] sites and 50,000 pages (fewer
//! under a storage limit), come to about 40 MB:
//!
//! ```text
//! DIR/wanted/
//!   wanted.json   the sites wanted, their searches' words and leads, and the pages found
//!   index-N/      the pages' index, rebuilt when pages come in
//! ```

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use plumb_core::article::Article;
use plumb_core::locks::{Locked, RwLocked};
use plumb_core::{domain_label, registrable_domain, Operators};
use plumb_crawl::{match_words, FollowTarget, Followed, Lead, WantedPage};
use plumb_index::pages::{Page, PageSearcher, WANTED_SET};
use plumb_index::SearchResults;
use serde::{Deserialize, Serialize};
use tracing::warn;

const DAY: u64 = 24 * 60 * 60;
/// How long a search's words are kept.
pub const WANT_KEPT: u64 = 30 * DAY;
/// Sites kept, the most recently wanted.
pub const MAX_SITES: usize = 2_000;
/// Searches kept for each site, the latest.
const MAX_WANTS: usize = 8;
/// Leads kept for each site, the best.
const MAX_SITE_LEADS: usize = 20;
/// A search's best sites whose pages it may want.
const SITES_PER_SEARCH: usize = 3;
/// Most words a search wants of a site.
const MAX_WANT_WORDS: usize = 8;
/// Soonest a site's links are followed again.
const LOOK_AGAIN: u64 = 60 * 60;
/// When a site's leads are followed again though no new search wants it.
const FOLLOW_LEADS_AGAIN: u64 = DAY;
/// Hops to another site a search's words may take from its own sites.
const MAX_HOPS: u8 = 1;
/// A page's views are this over its depth, so the nearer the homepage the
/// more it weighs, as with the subpages set.
const VIEWS: u64 = 1_000;
const FILE: &str = "wanted.json";
const INDEX_PREFIX: &str = "index-";

/// The sites a node's searches wanted pages of, and the pages found.
pub struct WantedPages {
    dir: PathBuf,
    state: RwLock<State>,
    saving: Mutex<()>,
    budget: Option<Arc<plumb_core::storage::StorageBudget>>,
    /// The pages' index and its number.
    index: RwLock<Option<(u64, Arc<PageSearcher>)>>,
}

#[derive(Debug, Default)]
struct State {
    saved: Saved,
    dirty: bool,
    /// Whether the index holds the pages as they are.
    built: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    sites: BTreeMap<String, Site>,
    pages: Vec<Found>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Site {
    homepage: String,
    /// The words of each search that wants a page of it, the latest last.
    wants: Vec<Want>,
    /// Links with a want's words left to follow, best first.
    leads: Vec<Lead>,
    /// When its links were last followed; 0 before.
    looked_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Want {
    words: Vec<String>,
    at: u64,
    /// Sites away from the search's own: 0 for them, 1 for a site one of
    /// their pages found links to.
    hops: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Found {
    #[serde(flatten)]
    page: WantedPage,
    /// When it was found, or found again: the least recent go first when
    /// too many are kept.
    at: u64,
}

impl Site {
    fn last_wanted(&self) -> u64 {
        self.wants.iter().map(|want| want.at).max().unwrap_or(0)
    }

    /// Whether its links are to be followed at `now`: a search wanted it
    /// since the last look, or leads are left from a look a day ago.
    fn is_due(&self, now: u64) -> bool {
        let since = now.saturating_sub(self.looked_at);
        !self.wants.is_empty()
            && since >= LOOK_AGAIN
            && (self.last_wanted() > self.looked_at
                || (!self.leads.is_empty() && since >= FOLLOW_LEADS_AGAIN))
    }
}

impl State {
    fn want(&mut self, domain: &str, homepage: &str, words: Vec<String>, hops: u8, now: u64) {
        let site = self.saved.sites.entry(domain.to_string()).or_default();
        if site.homepage.is_empty() {
            site.homepage = homepage.to_string();
        }
        let hops = match site.wants.iter().position(|want| want.words == words) {
            Some(i) => site.wants.remove(i).hops.min(hops),
            None => hops,
        };
        site.wants.push(Want {
            words,
            at: now,
            hops,
        });
        if site.wants.len() > MAX_WANTS {
            site.wants.remove(0);
        }
        self.dirty = true;
    }

    /// Keeps `page`, found at `now`; whether it is new.
    fn keep(&mut self, page: WantedPage, now: u64) -> bool {
        self.dirty = true;
        self.built = false;
        match self
            .saved
            .pages
            .iter_mut()
            .find(|found| found.page.url == page.url)
        {
            Some(found) => {
                *found = Found { page, at: now };
                false
            }
            None => {
                self.saved.pages.push(Found { page, at: now });
                true
            }
        }
    }

    /// Forgets searches older than [`WANT_KEPT`], sites no search wants,
    /// the least recently wanted sites past [`MAX_SITES`] and the least
    /// recently found pages past `max_pages`.
    fn prune(&mut self, now: u64, max_pages: usize) {
        let before = (self.saved.sites.len(), self.saved.pages.len());
        for site in self.saved.sites.values_mut() {
            site.wants.retain(|want| want.at + WANT_KEPT > now);
        }
        self.saved.sites.retain(|_, site| !site.wants.is_empty());
        if self.saved.sites.len() > MAX_SITES {
            let mut latest: Vec<u64> = self.saved.sites.values().map(Site::last_wanted).collect();
            latest.sort_unstable_by_key(|&at| Reverse(at));
            let oldest_kept = latest[MAX_SITES - 1];
            self.saved
                .sites
                .retain(|_, site| site.last_wanted() >= oldest_kept);
        }
        if self.saved.pages.len() > max_pages {
            self.saved.pages.sort_by_key(|found| Reverse(found.at));
            self.saved.pages.truncate(max_pages);
            self.built = false;
        }
        if before != (self.saved.sites.len(), self.saved.pages.len()) {
            self.dirty = true;
        }
    }
}

impl WantedPages {
    /// The store in `dir`, with what was saved there and its newest index.
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let saved: Saved = match std::fs::read(dir.join(FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                warn!("cannot read the pages wanted: {err}");
                Saved::default()
            }),
            Err(_) => Saved::default(),
        };
        let index = newest_index(&dir).and_then(|(number, path)| {
            PageSearcher::open(&path)
                .ok()
                .map(|searcher| (number, Arc::new(searcher)))
        });
        WantedPages {
            state: RwLock::new(State {
                built: index.is_some() || saved.pages.is_empty(),
                saved,
                dirty: false,
            }),
            dir,
            saving: Mutex::new(()),
            budget: None,
            index: RwLock::new(index),
        }
    }

    pub fn with_budget(mut self, budget: Option<Arc<plumb_core::storage::StorageBudget>>) -> Self {
        self.budget = budget;
        self
    }

    /// The pages found, to search next to the page sets.
    pub fn searcher(&self) -> Option<Arc<PageSearcher>> {
        self.index
            .read_locked()
            .as_ref()
            .map(|(_, searcher)| searcher.clone())
    }

    /// Notes the words of `query` besides a site's name for each of its
    /// best sites whose homepage `allowed` lets through, when `results`
    /// show no page of that site. A search for one of them by name wants
    /// nothing.
    pub fn note(
        &self,
        query: &str,
        results: &SearchResults,
        now: u64,
        allowed: impl Fn(&str) -> bool,
    ) {
        if Operators::parse(query).any() {
            return;
        }
        let words = match_words(query);
        let wants: Vec<(&plumb_index::Hit, Vec<String>)> = results
            .hits
            .iter()
            .take(SITES_PER_SEARCH)
            .map(|hit| {
                let name = match_words(&format!(
                    "{} {}",
                    domain_label(&hit.domain),
                    hit.title.as_deref().unwrap_or("")
                ));
                let mut want: Vec<String> = words.difference(&name).cloned().collect();
                want.sort();
                (hit, want)
            })
            .collect();
        if wants.iter().any(|(_, want)| want.is_empty()) {
            return;
        }
        let shown: HashSet<String> = results
            .pages
            .iter()
            .filter_map(|placed| registrable_domain(&placed.hit.page.url))
            .collect();
        let mut state = self.state.write_locked();
        for (hit, want) in wants {
            if want.len() <= MAX_WANT_WORDS && !shown.contains(&hit.domain) && allowed(&hit.url) {
                state.want(&hit.domain, &hit.url, want, 0, now);
            }
        }
    }

    /// The sites whose links are to be followed at `now`, the most recently
    /// wanted first, at most `max_sites` of them and `max_fetches` pages
    /// each.
    pub fn due(&self, now: u64, max_sites: usize, max_fetches: usize) -> Vec<FollowTarget> {
        let state = self.state.read_locked();
        let mut due: Vec<(&String, &Site)> = state
            .saved
            .sites
            .iter()
            .filter(|(_, site)| site.is_due(now))
            .collect();
        due.sort_by_key(|(_, site)| Reverse(site.last_wanted()));
        due.into_iter()
            .take(max_sites)
            .map(|(domain, site)| FollowTarget {
                domain: domain.clone(),
                homepage: site.homepage.clone(),
                leads: site.leads.clone(),
                wants: site.wants.iter().map(|want| want.words.clone()).collect(),
                max_fetches,
            })
            .collect()
    }

    /// Keeps what following `targets`' links found: the `https` pages
    /// `allowed` lets through, which answer the searches that wanted them,
    /// the leads left, and the other sites the pages link to with a
    /// search's words. Then forgets what [`State::prune`] does. Gives the
    /// number of new pages.
    pub fn apply(
        &self,
        targets: &[FollowTarget],
        followed: Vec<Followed>,
        now: u64,
        max_pages: usize,
        allowed: impl Fn(&str) -> bool,
    ) -> usize {
        let mut state = self.state.write_locked();
        let mut new = 0;
        for visit in followed {
            let Some(target) = targets.iter().find(|target| target.domain == visit.domain) else {
                continue;
            };
            let mut answered = HashSet::new();
            for (page, want) in visit.pages {
                if page.url.starts_with("https://") && allowed(&page.url) {
                    answered.insert(want);
                    new += usize::from(state.keep(page, now));
                }
            }
            let Some(site) = state.saved.sites.get_mut(&visit.domain) else {
                continue;
            };
            site.looked_at = now;
            site.leads = visit.leads;
            site.leads.truncate(MAX_SITE_LEADS);
            let hops = |words: &Vec<String>| {
                site.wants
                    .iter()
                    .find(|want| &want.words == words)
                    .map_or(MAX_HOPS, |want| want.hops)
            };
            let elsewhere: Vec<(Lead, Vec<String>, u8)> = visit
                .elsewhere
                .into_iter()
                .filter_map(|(lead, want)| {
                    let words = target.wants.get(want)?;
                    Some((lead, words.clone(), hops(words) + 1))
                })
                .filter(|(lead, _, hops)| *hops <= MAX_HOPS && allowed(&lead.url))
                .collect();
            let answered: Vec<&Vec<String>> = answered
                .into_iter()
                .filter_map(|want| target.wants.get(want))
                .collect();
            site.wants.retain(|want| !answered.contains(&&want.words));
            for (lead, words, hops) in elsewhere {
                let Some(domain) = registrable_domain(&lead.url).filter(|d| *d != visit.domain)
                else {
                    continue;
                };
                state.want(&domain, &format!("https://{domain}/"), words, hops, now);
                let leads = &mut state.saved.sites.entry(domain).or_default().leads;
                if !leads.iter().any(|known| known.url == lead.url) && leads.len() < MAX_SITE_LEADS
                {
                    leads.push(lead);
                }
            }
        }
        state.prune(now, max_pages);
        new
    }

    /// Saves what changed since the last save.
    pub fn save(&self) -> io::Result<()> {
        // One save at a time, so an older copy never lands after a newer one.
        let _saving = self.saving.locked();
        let bytes = {
            let mut state = self.state.write_locked();
            if !state.dirty {
                return Ok(());
            }
            state.dirty = false;
            serde_json::to_vec(&state.saved)?
        };
        let path = self.dir.join(FILE);
        let written = plumb_core::storage::create_directory(&self.dir, self.budget.as_ref())
            .map_err(io::Error::other)
            .and_then(|()| {
                plumb_core::storage::write_atomic(
                    &path,
                    &path.with_extension("json.tmp"),
                    &bytes,
                    self.budget.as_ref(),
                    None,
                )
            });
        if written.is_err() {
            self.state.write_locked().dirty = true;
        }
        written
    }

    /// Builds the pages' index again when pages came or went since the
    /// last build, and searches the new one. Building 50,000 pages takes a
    /// few seconds and up to about 100 MB.
    pub fn build_index(&self) -> Result<()> {
        let mut pages: Vec<WantedPage> = {
            let mut state = self.state.write_locked();
            if state.built {
                return Ok(());
            }
            state.built = true;
            state
                .saved
                .pages
                .iter()
                .map(|found| found.page.clone())
                .collect()
        };
        // Each set's most viewed page comes first (see
        // `build_page_index_with_budget`).
        pages.sort_by_key(|page| page.depth);
        let number = self.index.read_locked().as_ref().map_or(1, |(n, _)| n + 1);
        let built = if pages.is_empty() {
            Ok(None)
        } else {
            self.build(number, pages).map(Some)
        };
        let built = match built {
            Ok(built) => built,
            Err(err) => {
                self.state.write_locked().built = false;
                return Err(err);
            }
        };
        *self.index.write_locked() = built.map(|searcher| (number, Arc::new(searcher)));
        self.remove_indexes_but(number);
        Ok(())
    }

    fn build(&self, number: u64, pages: Vec<WantedPage>) -> Result<PageSearcher> {
        let dir = self.dir.join(format!("{INDEX_PREFIX}{number}"));
        plumb_core::storage::remove_directory(&dir, self.budget.as_deref())?;
        let pages = pages.into_iter().filter_map(|page| {
            Page::from_set(
                WANTED_SET,
                Article {
                    views: VIEWS / u64::from(page.depth.max(1)),
                    item: Some(page.url),
                    title: page.title,
                    description: page.description,
                    language: page.language,
                    ..Article::default()
                },
            )
        });
        plumb_index::pages::build_page_index_with_budget(
            &dir,
            pages,
            self.budget.clone(),
            &mut || Ok(()),
        )?;
        PageSearcher::open(&dir)
    }

    /// Deletes the indexes other than number `keep`; ones still open
    /// (Windows) stay until a later build.
    fn remove_indexes_but(&self, keep: u64) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let number = index_number(&entry.file_name().to_string_lossy());
            if number.is_some_and(|number| number != keep) {
                let _ =
                    plumb_core::storage::remove_directory(&entry.path(), self.budget.as_deref());
            }
        }
    }
}

fn index_number(name: &str) -> Option<u64> {
    name.strip_prefix(INDEX_PREFIX)?.parse().ok()
}

fn newest_index(dir: &Path) -> Option<(u64, PathBuf)> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            Some((
                index_number(&entry.file_name().to_string_lossy())?,
                entry.path(),
            ))
        })
        .max_by_key(|(number, _)| *number)
}

#[cfg(test)]
mod tests;
