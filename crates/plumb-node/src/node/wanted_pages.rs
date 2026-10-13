//! Follows the links of the sites a node's searches wanted pages of
//! ([`crate::wanted_pages`]) a few at a time, keeps the pages found and
//! builds their index again.
//!
//! Rounds wait while background updates are paused, as crawls do, and
//! count towards the day's download limit. A round fetches at most
//! [`SITES_PER_ROUND`] × [`FETCHES_PER_SITE`] pages, one site's at a time
//! a second apart; the store and its index take about 40 MB at most.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use plumb_core::now_unix;
use plumb_crawl::{follow_links, CrawlConfig};
use tracing::{info, warn};

use super::Inner;

/// Sites whose links are followed in one round.
const SITES_PER_ROUND: usize = 30;
/// Pages fetched on one site in a round.
const FETCHES_PER_SITE: usize = 12;
/// Sites visited at once.
const CONCURRENCY: usize = 8;
/// Wait between two rounds.
#[cfg(not(test))]
const ROUND_EVERY: Duration = Duration::from_secs(10 * 60);
#[cfg(test)]
const ROUND_EVERY: Duration = Duration::from_millis(200);

/// Runs until the node stops.
pub(super) async fn run(inner: Arc<Inner>) {
    // An index left unbuilt by the last run is built first.
    build(&inner).await;
    loop {
        if inner.pause().is_none() {
            round(&inner).await;
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
}

/// Most pages kept: fewer under a small storage limit.
fn max_pages(storage_limit_mb: u64) -> usize {
    match storage_limit_mb {
        1..1_000 => 5_000,
        1_000..8_000 => 20_000,
        _ => 50_000,
    }
}

async fn round(inner: &Arc<Inner>) {
    let targets = inner
        .wanted_pages
        .due(now_unix(), SITES_PER_ROUND, FETCHES_PER_SITE);
    if targets.is_empty() {
        return;
    }
    let cfg = CrawlConfig {
        use_system_proxy: inner.config.use_system_proxy,
        concurrency: CONCURRENCY,
        resolve: inner.config.page_reader.resolve.clone(),
        ..CrawlConfig::default()
    };
    let followed = tokio::select! {
        followed = follow_links(targets.clone(), &cfg) => followed,
        () = inner.stopped() => return,
    };
    if let Err(err) = inner.add_downloaded(cfg.downloaded.swap(0, Ordering::Relaxed)) {
        warn!("cannot count the downloads of wanted pages: {err:#}");
    }
    let fetched: usize = followed.iter().map(|visit| visit.fetched).sum();
    let max_pages = max_pages(inner.settings().storage_limit_mb);
    let new = inner
        .wanted_pages
        .apply(&targets, followed, now_unix(), max_pages, |url| {
            inner.may_want(url)
        });
    info!(
        "followed links on {} sites for this node's searches: {fetched} pages fetched, {new} new \
         pages kept",
        targets.len()
    );
    build(inner).await;
}

/// Saves the store and builds its index again off the async threads.
async fn build(inner: &Arc<Inner>) {
    let building = Arc::clone(inner);
    let built = tokio::task::spawn_blocking(move || {
        building.wanted_pages.save()?;
        building.wanted_pages.build_index()
    })
    .await;
    match built {
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
