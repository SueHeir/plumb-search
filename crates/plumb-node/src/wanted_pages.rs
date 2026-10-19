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
//! inner pages are. A page found that links to another site with the
//! words makes that site wanted too, one hop out from the search's own
//! sites, after them.
//!
//! The words, and the pages found, stay on the node in a directory only
//! its owner can read, and are forgotten after [`WANT_KEPT`]; the sites
//! see only the fetches, as a crawl's. Kept in memory and on disk,
//! at most [`MAX_SITES`] sites and 50,000 pages (fewer under a storage
//! limit), come to about 40 MB:
//!
//! ```text
//! DIR/wanted/
//!   sites.json   the sites wanted, their searches' words and leads
//!   pages.json   the pages found, and the number of their index
//!   index-N/     the pages' index, built again when pages come or go
//! ```
//!
//! An AI app the node's owner runs can ask for pages too, through the MCP
//! tool `fetch_pages`. Those go in a store of their own (`DIR/asked/`,
//! [`WantedPages::ask`]), kept for [`ASKED_KEPT`] and shown whether or
//! not the node looks for its searches' pages, so that turning
//! `--wanted-pages` off still hides every page a search led to.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use plumb_core::article::Article;
use plumb_core::locks::{Locked, RwLocked};
use plumb_core::{adult_level, domain_label, registrable_domain, AdultLevel, Operators};
use plumb_crawl::{match_words, FollowTarget, Followed, Lead, WantedPage};
use plumb_index::pages::{Page, PageSearcher, SetItem, WANTED_SET};
use plumb_index::SearchResults;
use serde::{Deserialize, Serialize};
use tracing::warn;

const DAY: u64 = 24 * 60 * 60;
/// How long a search's words, and a page found, are kept.
pub const WANT_KEPT: u64 = 30 * DAY;
/// How long the pages an AI app asked for, and its words, are kept.
pub const ASKED_KEPT: u64 = 365 * DAY;
/// Sites kept: the searches' own before those one hop out, the most
/// recently wanted first.
pub const MAX_SITES: usize = 2_000;
/// Searches kept for each site, the latest.
const MAX_WANTS: usize = 8;
/// Leads kept for each site, the best.
const MAX_SITE_LEADS: usize = 20;
/// A search's best sites whose pages it may want.
const SITES_PER_SEARCH: usize = 3;
/// Most words a search wants of a site.
pub const MAX_WANT_WORDS: usize = 8;
/// Soonest a site's links are followed again.
const LOOK_AGAIN: u64 = 60 * 60;
/// When a site's leads are followed again though no new search wants it.
const FOLLOW_LEADS_AGAIN: u64 = DAY;
/// Hops to another site a search's words may take from its own sites.
const MAX_HOPS: u8 = 1;
/// A page's views are this over its depth, so the nearer the homepage the
/// more it weighs, as with the subpages set.
const VIEWS: u64 = 1_000;
/// Pages fetched on a site when an AI app asks for some.
pub const ASKED_FETCHES: usize = 12;
const SITES_FILE: &str = "sites.json";
const PAGES_FILE: &str = "pages.json";
const INDEX_PREFIX: &str = "index-";

/// The sites a node's searches wanted pages of, and the pages found. The
/// sites, which every search may change, and the pages, which only the
/// background job does, are locked apart.
pub struct WantedPages {
    dir: PathBuf,
    sites: RwLock<Sites>,
    pages: RwLock<Pages>,
    saving: Mutex<()>,
    /// Held while the index is built, so two builds never take one number.
    building: Mutex<()>,
    budget: Option<Arc<plumb_core::storage::StorageBudget>>,
    /// The pages' index and its number.
    index: RwLock<Option<(u64, Arc<PageSearcher>)>>,
    /// How long words and pages are kept: [`WANT_KEPT`] or [`ASKED_KEPT`].
    kept: u64,
}

#[derive(Debug, Default)]
struct Sites {
    sites: BTreeMap<String, Site>,
    dirty: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Pages {
    pages: BTreeMap<String, Found>,
    /// The number of the index built of these pages; 0 while there is none.
    index: u64,
    #[serde(skip)]
    dirty: bool,
}

/// A site the store holds words or pages of, as
/// [`WantedPages::summary`] lists it. Times are Unix seconds, 0 for never.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WantedSite {
    pub site: String,
    /// When pages of it were last wanted.
    pub wanted_at: u64,
    /// When its links were last followed.
    pub looked_at: u64,
    /// The words still looked for: a want leaves once a page answers it.
    pub words: Vec<String>,
    /// Its pages kept.
    pub pages: usize,
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
    /// When a search wanted it: for a site one hop out, when the search
    /// that led there did.
    at: u64,
    /// Sites away from the search's own: 0 for them, 1 for a site one of
    /// their pages found links to.
    hops: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    /// Which sites come first: the ones searches named, then the most
    /// recently wanted.
    fn order(&self) -> (u8, Reverse<u64>) {
        let hops = self.wants.iter().map(|want| want.hops).min();
        (hops.unwrap_or(MAX_HOPS), Reverse(self.last_wanted()))
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

impl Sites {
    fn want(&mut self, domain: &str, homepage: &str, want: Want) {
        // Searches for ever new sites wait for the next prune.
        if self.sites.len() >= 2 * MAX_SITES && !self.sites.contains_key(domain) {
            return;
        }
        let site = self.sites.entry(domain.to_string()).or_default();
        if site.homepage.is_empty() {
            site.homepage = homepage.to_string();
        }
        let want = match site.wants.iter().position(|old| old.words == want.words) {
            Some(i) => {
                let old = site.wants.remove(i);
                Want {
                    at: old.at.max(want.at),
                    hops: old.hops.min(want.hops),
                    ..want
                }
            }
            None => want,
        };
        site.wants.push(want);
        if site.wants.len() > MAX_WANTS {
            site.wants.remove(0);
        }
        self.dirty = true;
    }

    /// Forgets searches older than `kept`, sites no search wants any
    /// more, and the sites past [`MAX_SITES`].
    fn prune(&mut self, now: u64, kept: u64) {
        let count = |sites: &BTreeMap<String, Site>| -> (usize, usize) {
            let wants = sites.values().map(|site| site.wants.len()).sum();
            (sites.len(), wants)
        };
        let before = count(&self.sites);
        for site in self.sites.values_mut() {
            site.wants.retain(|want| want.at + kept > now);
        }
        self.sites.retain(|_, site| !site.wants.is_empty());
        if self.sites.len() > MAX_SITES {
            let mut order: Vec<_> = self.sites.values().map(Site::order).collect();
            order.sort_unstable();
            let last_kept = order[MAX_SITES - 1];
            self.sites.retain(|_, site| site.order() <= last_kept);
        }
        if count(&self.sites) != before {
            self.dirty = true;
        }
    }
}

impl Pages {
    /// Keeps `page`, found at `now`; whether it is new or changed, which
    /// calls for a new index.
    fn keep(&mut self, page: WantedPage, now: u64) -> bool {
        let changed = self.pages.get(&page.url).is_none_or(|old| old.page != page);
        self.pages.insert(page.url.clone(), Found { page, at: now });
        if changed {
            self.index = 0;
        }
        self.dirty = true;
        changed
    }

    /// Forgets the pages not found again in `kept`: which pages a node
    /// went looking for tells what its people searched. Then the least
    /// recently found past `max_pages`.
    fn prune(&mut self, now: u64, max_pages: usize, kept: u64) {
        let before = self.pages.len();
        self.pages.retain(|_, found| found.at + kept > now);
        if self.pages.len() != before {
            self.index = 0;
            self.dirty = true;
        }
        if self.pages.len() <= max_pages {
            return;
        }
        let mut found: Vec<(u64, String)> = self
            .pages
            .values()
            .map(|found| (found.at, found.page.url.clone()))
            .collect();
        found.sort_unstable();
        for (_, url) in &found[..found.len() - max_pages] {
            self.pages.remove(url);
        }
        self.index = 0;
        self.dirty = true;
    }
}

impl WantedPages {
    /// The store in `dir`, with what was saved there, and the index of its
    /// pages when it was built of the pages saved.
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let sites = read_json(&dir.join(SITES_FILE)).unwrap_or_default();
        let pages: Pages = read_json(&dir.join(PAGES_FILE)).unwrap_or_default();
        let index = newest_index(&dir)
            .filter(|(number, _)| *number == pages.index)
            .and_then(|(number, path)| {
                let searcher = PageSearcher::open(&path).ok()?;
                Some((number, Arc::new(searcher)))
            });
        WantedPages {
            sites: RwLock::new(Sites {
                sites,
                dirty: false,
            }),
            pages: RwLock::new(pages),
            index: RwLock::new(index),
            dir,
            saving: Mutex::new(()),
            building: Mutex::new(()),
            budget: None,
            kept: WANT_KEPT,
        }
    }

    /// Keeps words and pages `kept` seconds instead of [`WANT_KEPT`].
    pub fn keeping(mut self, kept: u64) -> Self {
        self.kept = kept;
        self
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
        let mut sites = self.sites.write_locked();
        for (hit, words) in wants {
            let site = registrable_domain(&hit.domain).unwrap_or_else(|| hit.domain.clone());
            if words.len() <= MAX_WANT_WORDS && !shown.contains(&site) && allowed(&hit.url) {
                let want = Want {
                    words,
                    at: now,
                    hops: 0,
                };
                sites.want(&hit.domain, &hit.url, want);
            }
        }
    }

    /// Forgets old searches and pages, and the sites and pages past the
    /// most kept.
    pub fn prune(&self, now: u64, max_pages: usize) {
        self.sites.write_locked().prune(now, self.kept);
        self.pages.write_locked().prune(now, max_pages, self.kept);
    }

    /// Notes that pages of the site at `homepage` with `words` are wanted,
    /// as an AI app asked through `fetch_pages`, and gives the visit that
    /// looks for them now: the site's leads from earlier visits first.
    pub fn ask(&self, domain: &str, homepage: &str, words: Vec<String>, now: u64) -> FollowTarget {
        let mut sites = self.sites.write_locked();
        let want = Want {
            words: words.clone(),
            at: now,
            hops: 0,
        };
        sites.want(domain, homepage, want);
        FollowTarget {
            domain: domain.to_string(),
            homepage: homepage.to_string(),
            leads: sites
                .sites
                .get(domain)
                .map(|site| site.leads.clone())
                .unwrap_or_default(),
            wants: vec![words],
            max_fetches: ASKED_FETCHES,
        }
    }

    /// The pages kept of the site `domain`, best first: nearest its
    /// homepage, then by address.
    pub fn pages_of(&self, domain: &str) -> Vec<WantedPage> {
        let mut pages: Vec<WantedPage> = self
            .pages
            .read_locked()
            .pages
            .values()
            .filter(|found| registrable_domain(&found.page.url).as_deref() == Some(domain))
            .map(|found| found.page.clone())
            .collect();
        pages.sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.url.cmp(&b.url)));
        pages
    }

    /// Each site wanted, the most recently first, with the words still
    /// looked for and the number of its pages kept; and the pages kept in all.
    pub fn summary(&self) -> (Vec<WantedSite>, usize) {
        let pages = self.pages.read_locked();
        let mut kept: BTreeMap<String, usize> = BTreeMap::new();
        for found in pages.pages.values() {
            if let Some(domain) = registrable_domain(&found.page.url) {
                *kept.entry(domain).or_default() += 1;
            }
        }
        let sites = self.sites.read_locked();
        let mut wanted: Vec<WantedSite> = sites
            .sites
            .iter()
            .map(|(domain, site)| WantedSite {
                site: domain.clone(),
                wanted_at: site.last_wanted(),
                looked_at: site.looked_at,
                words: site.wants.iter().map(|want| want.words.join(" ")).collect(),
                pages: kept.remove(domain).unwrap_or(0),
            })
            .collect();
        // Sites whose words were all answered keep their pages.
        wanted.extend(kept.into_iter().map(|(site, pages)| WantedSite {
            site,
            pages,
            ..WantedSite::default()
        }));
        wanted.sort_by_key(|site| Reverse(site.wanted_at.max(site.looked_at)));
        (wanted, pages.pages.len())
    }

    /// The sites whose links are to be followed at `now`, whose homepage
    /// `allowed` lets through: the searches' own first, then the most
    /// recently wanted, at most `max_sites` of them and `max_fetches`
    /// pages each.
    pub fn due(
        &self,
        now: u64,
        max_sites: usize,
        max_fetches: usize,
        allowed: impl Fn(&str) -> bool,
    ) -> Vec<FollowTarget> {
        let sites = self.sites.read_locked();
        let mut due: Vec<(&String, &Site)> = sites
            .sites
            .iter()
            .filter(|(_, site)| site.is_due(now) && allowed(&site.homepage))
            .collect();
        due.sort_by_key(|(_, site)| site.order());
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
    /// `allowed` lets through and not plainly adult, which answer the
    /// searches that wanted them,
    /// the leads left, and the other sites the pages link to with a
    /// search's words. Gives the number of pages new or changed.
    pub fn apply(
        &self,
        targets: &[FollowTarget],
        followed: Vec<Followed>,
        now: u64,
        allowed: impl Fn(&str) -> bool,
    ) -> usize {
        let target = |domain: &str| targets.iter().find(|target| target.domain == domain);
        let explicit = |page: &WantedPage| {
            let domain = registrable_domain(&page.url).unwrap_or_default();
            let texts = [page.title.as_str()]
                .into_iter()
                .chain(page.description.as_deref());
            adult_level(&domain, texts) == AdultLevel::Explicit
        };
        let mut changed = 0;
        let mut answered: HashSet<(String, Vec<String>)> = HashSet::new();
        {
            let mut pages = self.pages.write_locked();
            for visit in &followed {
                let Some(target) = target(&visit.domain) else {
                    continue;
                };
                for (page, want) in &visit.pages {
                    if page.url.starts_with("https://") && allowed(&page.url) && !explicit(page) {
                        changed += usize::from(pages.keep(page.clone(), now));
                        let words = target.wants.get(*want).cloned().unwrap_or_default();
                        answered.insert((visit.domain.clone(), words));
                    }
                }
            }
        }
        let mut sites = self.sites.write_locked();
        sites.dirty = true;
        for visit in followed {
            let Some(target) = target(&visit.domain) else {
                continue;
            };
            let Some(site) = sites.sites.get_mut(&visit.domain) else {
                continue;
            };
            site.looked_at = now;
            site.leads = visit.leads;
            site.leads.retain(|lead| allowed(&lead.url));
            site.leads.truncate(MAX_SITE_LEADS);
            // A site one hop out takes the words and time of the search
            // that led there.
            let elsewhere: Vec<(Lead, Want)> = visit
                .elsewhere
                .into_iter()
                .filter_map(|(lead, want)| {
                    let words = target.wants.get(want)?;
                    let cause = site.wants.iter().find(|w| &w.words == words)?;
                    let want = Want {
                        hops: cause.hops + 1,
                        ..cause.clone()
                    };
                    (want.hops <= MAX_HOPS && allowed(&lead.url)).then_some((lead, want))
                })
                .collect();
            let domain = visit.domain;
            site.wants
                .retain(|want| !answered.contains(&(domain.clone(), want.words.clone())));
            let own = registrable_domain(&domain);
            for (lead, want) in elsewhere {
                let Some(other) = registrable_domain(&lead.url).filter(|d| Some(d) != own.as_ref())
                else {
                    continue;
                };
                sites.want(&other, &format!("https://{other}/"), want);
                let Some(site) = sites.sites.get_mut(&other) else {
                    continue;
                };
                let leads = &mut site.leads;
                if !leads.iter().any(|known| known.url == lead.url) && leads.len() < MAX_SITE_LEADS
                {
                    leads.push(lead);
                }
            }
        }
        changed
    }

    /// Saves what changed since the last save, privately: the searches'
    /// words are no one else's business.
    pub fn save(&self) -> io::Result<()> {
        // One save at a time, so an older copy never lands after a newer one.
        let _saving = self.saving.locked();
        // Copied under the locks, written outside them.
        let sites = {
            let mut sites = self.sites.write_locked();
            let copy = sites.dirty.then(|| sites.sites.clone());
            sites.dirty = false;
            copy
        };
        let pages = {
            let mut pages = self.pages.write_locked();
            let copy = pages.dirty.then(|| Pages {
                pages: pages.pages.clone(),
                index: pages.index,
                dirty: false,
            });
            pages.dirty = false;
            copy
        };
        let written = self.write_files(sites.as_ref(), pages.as_ref());
        if written.is_err() {
            self.sites.write_locked().dirty |= sites.is_some();
            self.pages.write_locked().dirty |= pages.is_some();
        }
        written
    }

    /// Creates the store's directory, readable by the node's owner only:
    /// the index holds the pages found too.
    fn create_dir(&self) -> io::Result<()> {
        plumb_core::storage::create_directory(&self.dir, self.budget.as_ref())
            .map_err(io::Error::other)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    fn write_files(
        &self,
        sites: Option<&BTreeMap<String, Site>>,
        pages: Option<&Pages>,
    ) -> io::Result<()> {
        let files = [
            (SITES_FILE, sites.map(serde_json::to_vec).transpose()?),
            (PAGES_FILE, pages.map(serde_json::to_vec).transpose()?),
        ];
        for (name, bytes) in files {
            let Some(bytes) = bytes else {
                continue;
            };
            self.create_dir()?;
            let path = self.dir.join(name);
            plumb_core::storage::write_atomic_private(
                &path,
                &path.with_extension("json.tmp"),
                &bytes,
                self.budget.as_ref(),
            )?;
        }
        Ok(())
    }

    /// Builds the pages' index again when pages came or went since the
    /// last build, and searches the new one. Building 50,000 pages takes a
    /// few seconds and up to about 100 MB.
    pub fn build_index(&self) -> Result<()> {
        let _building = self.building.locked();
        let mut pages: Vec<WantedPage> = {
            let pages = self.pages.read_locked();
            let built = self.index.read_locked().as_ref().map(|(n, _)| *n);
            let current = match built {
                Some(built) => built == pages.index,
                None => pages.pages.is_empty(),
            };
            if current {
                return Ok(());
            }
            pages
                .pages
                .values()
                .map(|found| found.page.clone())
                .collect()
        };
        // Each set's most viewed page comes first (see
        // `build_page_index_with_budget`).
        pages.sort_by_key(|page| page.depth);
        let number = newest_index(&self.dir).map_or(1, |(n, _)| n + 1);
        let built = match pages.is_empty() {
            true => None,
            false => Some((number, Arc::new(self.build(number, pages)?))),
        };
        let kept = built.as_ref().map_or(0, |(n, _)| *n);
        *self.index.write_locked() = built;
        {
            let mut pages = self.pages.write_locked();
            pages.index = kept;
            pages.dirty = true;
        }
        self.remove_indexes_but(kept);
        Ok(())
    }

    fn build(&self, number: u64, pages: Vec<WantedPage>) -> Result<PageSearcher> {
        self.create_dir()?;
        let dir = self.dir.join(format!("{INDEX_PREFIX}{number}"));
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
            pages.map(|page| SetItem::of_page(&page)),
            self.budget.clone(),
            &|_| true,
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

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes)
        .inspect_err(|err| warn!("cannot read {}: {err}", path.display()))
        .ok()
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
