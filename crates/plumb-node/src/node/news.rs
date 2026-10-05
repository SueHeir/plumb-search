//! Checks the feeds of the sites a node watches ([`crate::news`]) when
//! they are due, keeps their recent headlines, and shares the new ones
//! with the network in a batch, as a crawl's homepages are shared.
//!
//! Checks wait while background updates are paused, as crawls do, and
//! count towards the day's download limit.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use plumb_core::now_unix;
use plumb_crawl::{check_feeds, CrawlConfig};
use tracing::{info, warn};

use super::Inner;

/// Feeds checked in one round, best-ranked first; the rest wait for the
/// next round.
const MAX_PER_ROUND: usize = 500;
/// Longest wait between two looks at what is due.
#[cfg(not(test))]
const LOOK_EVERY: Duration = Duration::from_secs(10 * 60);
#[cfg(test)]
const LOOK_EVERY: Duration = Duration::from_millis(200);
/// Feeds fetched at once.
const CONCURRENCY: usize = 16;

/// Runs until the node stops.
pub(super) async fn run(inner: Arc<Inner>) {
    loop {
        let now = now_unix();
        inner.news.prune(now);
        if inner.config.news_feeds > 0 && inner.pause().is_none() {
            check_due(&inner, now).await;
        }
        if let Err(err) = inner.news.save() {
            warn!("cannot save the recent headlines: {err}");
        }
        let wait = inner
            .news
            .next_due()
            .filter(|_| inner.config.news_feeds > 0)
            .map(|at| Duration::from_secs(at.saturating_sub(now_unix()).max(1)))
            .unwrap_or(LOOK_EVERY)
            .min(LOOK_EVERY);
        tokio::select! {
            () = inner.stopped() => break,
            () = tokio::time::sleep(wait) => {}
        }
    }
    if let Err(err) = inner.news.save() {
        warn!("cannot save the recent headlines: {err}");
    }
}

/// Checks the feeds due at `now`, keeps what they say, and shares the new
/// headlines.
async fn check_due(inner: &Arc<Inner>, now: u64) {
    let due = inner.news.due(now, MAX_PER_ROUND);
    if due.is_empty() {
        return;
    }
    let cfg = CrawlConfig {
        use_system_proxy: inner.config.use_system_proxy,
        concurrency: CONCURRENCY,
        ..CrawlConfig::default()
    };
    let n = due.len();
    let checks = tokio::select! {
        checks = check_feeds(due, &cfg) => checks,
        () = inner.stopped() => return,
    };
    if let Err(err) = inner.add_downloaded(cfg.downloaded.swap(0, Ordering::Relaxed)) {
        warn!("cannot count the feeds' downloads: {err:#}");
    }
    let fresh = inner.news.apply(checks, now_unix());
    let (watched, feeds) = inner.news.watched();
    info!(
        "checked {n} feeds: {} sites had new headlines; {feeds} of the {watched} sites watched \
         have a feed, {} headlines kept",
        fresh.len(),
        inner.news.len()
    );
    if fresh.is_empty() {
        return;
    }
    if let Some(net) = super::network::handle(inner) {
        if let Err(err) = net.publish(fresh).await {
            warn!("cannot share new headlines with the network: {err:#}");
        }
    }
}
