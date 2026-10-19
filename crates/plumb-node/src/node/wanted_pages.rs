//! Follows the links of the sites a node's searches wanted pages of
//! ([`crate::wanted_pages`]) a few at a time, keeps the pages found and
//! builds their index again; and the same for the pages AI apps asked
//! for, in their own store.
//!
//! Rounds wait while background updates are paused, as crawls do, and
//! count towards the day's download limit. A round fetches at most
//! [`SITES_PER_ROUND`] × [`FETCHES_PER_SITE`] pages, one site's at a time
//! a second apart; each store and its index take about 40 MB at most.
//!
//! An AI app's `fetch_pages` ([`Inner::fetch_asked`]) visits its site at
//! once and builds the index again before it answers. One such visit, or
//! one round on the asked store, runs at a time (`Inner::asking`), so no
//! site is visited twice at once and the store has one writer. The
//! owner's own app asked, so it does not wait for background updates,
//! only for the download and storage limits.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use plumb_core::locks::RwLocked;
use plumb_core::now_unix;
use plumb_crawl::{follow_links, CrawlConfig, Lead, WantedPage};
use serde::Serialize;
use tracing::{info, warn};

use super::Inner;
use crate::wanted_pages::WantedPages;

/// Sites whose links are followed in one round.
const SITES_PER_ROUND: usize = 30;
/// Pages fetched on one site in a round.
const FETCHES_PER_SITE: usize = 12;
/// Sites visited at once.
const CONCURRENCY: usize = 8;
/// Longest `fetch_pages` waits for one page, so that a visit of
/// [`crate::wanted_pages::ASKED_FETCHES`] pages ends within a few minutes.
const ASKED_PAGE_TIME: Duration = Duration::from_secs(15);
/// Longest `fetch_pages` waits for another visit to end.
const ASKED_WAIT: Duration = Duration::from_secs(30);
/// Wait between two rounds.
#[cfg(not(test))]
const ROUND_EVERY: Duration = Duration::from_secs(10 * 60);
#[cfg(test)]
const ROUND_EVERY: Duration = Duration::from_millis(200);

/// Which store a round works on.
type Store = fn(&Inner) -> &WantedPages;

/// Runs until the node stops. Turned off, or on a node that only crawls,
/// it only forgets old searches and pages on time. The pages AI apps
/// asked for have their links followed again whether or not the node
/// looks for its searches' pages.
pub(super) async fn run(inner: Arc<Inner>) {
    // Each store, whether it is on, and whether it is the asked one.
    let stores: [(Store, bool, bool); 2] = [
        (
            |inner| &inner.wanted_pages,
            inner.config.wanted_pages,
            false,
        ),
        (|inner| &inner.asked_pages, true, true),
    ];
    loop {
        let max_pages = max_pages(inner.settings().storage_limit_mb);
        // Pages found are shown next to the page sets' only.
        let shown = inner.pages.read_locked().is_some();
        let paused = inner.pause().is_some();
        for (store, on, asked) in stores {
            let _one_visit = match asked {
                true => Some(inner.asking.lock().await),
                false => None,
            };
            store(&inner).prune(now_unix(), max_pages);
            if on && shown && !paused {
                round(&inner, store).await;
            }
            keep(&inner, store).await;
        }
        tokio::select! {
            () = inner.stopped() => break,
            () = tokio::time::sleep(ROUND_EVERY) => {}
        }
    }
}

impl Inner {
    /// Whether the node may fetch and keep the page at `url` for its
    /// searches: neither the denylist nor the adult blocklist takes it off.
    pub(super) fn may_want(&self, url: &str) -> bool {
        let adult = |domain: &String| self.adult_list().is_some_and(|list| list.contains(domain));
        !self.denies(url) && plumb_core::registrable_domain(url).is_some_and(|d| !adult(&d))
    }

    fn follow_config(&self) -> CrawlConfig {
        CrawlConfig {
            use_system_proxy: self.config.use_system_proxy,
            concurrency: CONCURRENCY,
            resolve: self.config.page_reader.resolve.clone(),
            ..CrawlConfig::default()
        }
    }

    /// `fetch_pages`: notes that an AI app wants pages of the site at
    /// `start` (a homepage, or a page to start from) with `words`, follows
    /// its links for them now, and keeps and indexes what it finds. Blocks
    /// for up to a few minutes: call it off async threads.
    pub(super) fn fetch_asked(&self, start: &str, words: Vec<String>) -> Result<Fetched> {
        let url = url::Url::parse(start)?;
        let Some(domain) = plumb_core::registrable_domain(start) else {
            bail!("{start} is not a site's address");
        };
        if self.pages.read_locked().is_none() {
            bail!("this node shows pages found next to its page sets, and it holds none yet");
        }
        if !self.may_want(start) {
            bail!("this node's denylist or adult blocklist takes {domain} off");
        }
        let limits = self
            .download_limit_pause()
            .or_else(|| self.storage_pause(|| self.disk_used_shown()));
        if let Some(pause) = limits {
            bail!("{}", pause.reason);
        }
        let runtime = tokio::runtime::Handle::current();
        let Ok(_one_visit) = runtime.block_on(tokio::time::timeout(ASKED_WAIT, self.asking.lock()))
        else {
            bail!("this node is busy fetching another site's pages; try again in a minute");
        };
        let homepage = format!("{}://{}/", url.scheme(), url.host_str().unwrap_or(&domain));
        let given = (url.path() != "/" || url.query().is_some()).then(|| Lead {
            url: start.to_string(),
            label: words.join(" "),
            depth: 1,
        });
        let store = &self.asked_pages;
        let target = store.ask(&domain, &homepage, given, words.clone(), now_unix());
        let cfg = CrawlConfig {
            target_deadline: ASKED_PAGE_TIME,
            ..self.follow_config()
        };
        let may_want = |url: &str| self.may_want(url);
        let followed = runtime.block_on(async {
            tokio::select! {
                followed = follow_links(vec![target.clone()], &cfg, &may_want) => Some(followed),
                () = self.stopped() => None,
            }
        });
        let Some(followed) = followed else {
            bail!("the node is stopping");
        };
        if let Err(err) = self.add_downloaded(cfg.downloaded.swap(0, Ordering::Relaxed)) {
            warn!("cannot count the downloads of pages an AI app asked for: {err:#}");
        }
        let fetched = followed.iter().map(|visit| visit.fetched).sum();
        let new = store.apply(
            std::slice::from_ref(&target),
            followed,
            now_unix(),
            may_want,
        );
        store.build_index()?;
        store.save()?;
        self.journal.info(format!(
            "An AI app asked for pages of {domain} about \"{}\": {fetched} fetched, {new} new",
            words.join(" ")
        ));
        let pages = store.pages_of(&domain).into_iter();
        Ok(Fetched {
            site: domain,
            words,
            fetched,
            new,
            pages: pages
                .filter(|page| !self.denies(&page.url))
                .take(MAX_PAGES_LISTED)
                .collect(),
        })
    }
}

/// What `fetch_pages` did.
#[derive(Debug, Clone, Serialize)]
pub struct Fetched {
    pub site: String,
    /// The words looked for, as links and pages are matched by them.
    pub words: Vec<String>,
    /// Pages fetched.
    pub fetched: usize,
    /// Pages new or changed among those kept.
    pub new: usize,
    /// The pages now kept of the site, nearest its homepage first.
    pub pages: Vec<WantedPage>,
}

/// Most of a site's pages `fetch_pages` lists.
const MAX_PAGES_LISTED: usize = 20;

/// Most pages kept: fewer under a small storage limit.
fn max_pages(storage_limit_mb: u64) -> usize {
    match storage_limit_mb {
        1..1_000 => 5_000,
        1_000..8_000 => 20_000,
        _ => 50_000,
    }
}

async fn round(inner: &Arc<Inner>, store: Store) {
    let may_want = |url: &str| inner.may_want(url);
    let targets = store(inner).due(now_unix(), SITES_PER_ROUND, FETCHES_PER_SITE, may_want);
    if targets.is_empty() {
        return;
    }
    let cfg = inner.follow_config();
    let followed = tokio::select! {
        followed = follow_links(targets.clone(), &cfg, &may_want) => followed,
        () = inner.stopped() => return,
    };
    if let Err(err) = inner.add_downloaded(cfg.downloaded.swap(0, Ordering::Relaxed)) {
        warn!("cannot count the downloads of wanted pages: {err:#}");
    }
    let fetched: usize = followed.iter().map(|visit| visit.fetched).sum();
    let sites = targets.len();
    let applying = Arc::clone(inner);
    let changed = tokio::task::spawn_blocking(move || {
        let may_want = |url: &str| applying.may_want(url);
        store(&applying).apply(&targets, followed, now_unix(), may_want)
    })
    .await
    .unwrap_or_default();
    info!(
        "followed links on {sites} sites for this node's searches: {fetched} pages fetched, \
         {changed} pages new or changed"
    );
}

/// Builds the pages' index again if they changed, and saves the store,
/// off the async threads.
async fn keep(inner: &Arc<Inner>, store: Store) {
    let keeping = Arc::clone(inner);
    let kept = tokio::task::spawn_blocking(move || {
        let built = store(&keeping).build_index();
        store(&keeping).save()?;
        built
    })
    .await;
    match kept {
        Ok(Ok(())) => {}
        Ok(Err(err)) => warn!("cannot keep the wanted pages: {err:#}"),
        Err(err) => warn!("cannot keep the wanted pages: {err}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_small_storage_limit_keeps_fewer_pages() {
        assert_eq!(super::max_pages(0), 50_000);
        assert_eq!(super::max_pages(500), 5_000);
        assert_eq!(super::max_pages(2_000), 20_000);
        assert_eq!(super::max_pages(100_000), 50_000);
    }
}
