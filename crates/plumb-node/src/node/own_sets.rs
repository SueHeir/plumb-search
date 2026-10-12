//! Runs the plugins that build sets ahead of time
//! ([`crate::plugins::publish`]), each on its own schedule, until the node
//! stops.
//!
//! A plugin first runs a little after the node starts, then again its
//! `publish.every` after its last run. A run that failed is tried again
//! after an hour, or sooner when its schedule is shorter. The sets are
//! signed with the owner's publisher key, made on the first run.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use plumb_format::PublicKey;
use tracing::{info, warn};

use super::Inner;
use crate::plugins::publish::{own_key, publish_set, read_note, Owner};
use crate::plugins::{Plugin, Plugins};

/// How long after the node starts a plugin first runs, so a start is not
/// slowed by plugins fetching.
#[cfg(not(test))]
const FIRST_WAIT: Duration = Duration::from_secs(2 * 60);
#[cfg(test)]
const FIRST_WAIT: Duration = Duration::from_millis(100);
/// How soon a run that failed is tried again, at most.
const RETRY_WAIT: Duration = Duration::from_secs(3600);

/// Keeps the sets `plugins` build up to date until the node stops.
pub(super) async fn run(inner: Arc<Inner>, plugins: Plugins) {
    let mut sets = std::collections::HashSet::new();
    let publishing: Vec<_> = plugins
        .publishing()
        .into_iter()
        .filter(|plugin| {
            let set = plugin
                .manifest
                .publish
                .as_ref()
                .map(|publish| publish.set_name())
                .unwrap_or_default();
            let first = sets.insert(set.clone());
            if !first {
                warn!(
                    "plugin {} builds no set: another plugin builds {set}",
                    plugin.id
                );
            }
            first
        })
        .collect();
    if publishing.is_empty() {
        return;
    }
    let data = inner.paths.data.clone();
    let key = match tokio::task::spawn_blocking(move || own_key(&data)).await {
        Ok(Ok(key)) => Arc::new(key),
        Ok(Err(err)) => {
            warn!("plugins build no sets: no publisher key: {err:#}");
            return;
        }
        Err(err) => {
            warn!("plugins build no sets: {err}");
            return;
        }
    };
    info!(
        "{} plugins build sets, signed with this node's publisher key {}",
        publishing.len(),
        PublicKey::of(&key)
    );
    let started = tokio::time::Instant::now();
    // When each plugin last ran, should its note fail to be written.
    let mut ran_at: HashMap<String, u64> = HashMap::new();
    loop {
        let now = unix_now();
        let Some((wait, plugin)) = publishing
            .iter()
            .map(|plugin| {
                let ran = ran_at.get(&plugin.id).copied().unwrap_or(0);
                (due_in(&inner, plugin, ran, now), plugin)
            })
            .min_by_key(|(wait, _)| *wait)
        else {
            return;
        };
        let wait = wait.max(FIRST_WAIT.saturating_sub(started.elapsed()));
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = inner.stopped() => return,
        }
        let ran = {
            let (inner, plugin, key) = (inner.clone(), plugin.clone(), key.clone());
            let runtime = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                let owner = Owner {
                    data: &inner.paths.data,
                    key: &key,
                    denylist: inner.denylist(),
                    budget: inner.storage.as_deref(),
                    stop: Some(inner.stopped.clone()),
                };
                publish_set(&plugin, &owner, unix_now(), &runtime)
            })
        };
        // A run notices the node stopping at its next request or batch;
        // the node does not wait for it.
        let ran = tokio::select! {
            ran = ran => ran,
            () = inner.stopped() => return,
        };
        ran_at.insert(plugin.id.clone(), unix_now());
        let name = &plugin.manifest.name;
        match ran {
            Ok(Ok(ran)) => {
                let line = format!(
                    "Plugin {name} built its set: {} items ({} new or changed{}{})",
                    ran.items,
                    ran.emitted,
                    match ran.refused {
                        0 => String::new(),
                        n => format!(", {n} refused"),
                    },
                    match ran.denied {
                        0 => String::new(),
                        n => format!(", {n} on the denylist"),
                    },
                );
                info!("{line}");
                if let Some(why) = &ran.first_refused {
                    warn!("plugin {name}: the first item refused: {why}");
                }
                inner.journal.info(line);
            }
            Ok(Err(err)) => {
                warn!("plugin {name} could not build its set: {err:#}");
                inner
                    .journal
                    .warning(format!("Plugin {name} could not build its set: {err}"));
            }
            Err(err) => warn!("plugin {name} could not build its set: {err}"),
        }
    }
}

/// How long until `plugin`, which this node last ran at `ran_at` (Unix
/// seconds, 0 for not yet), is due to run.
fn due_in(inner: &Inner, plugin: &Plugin, ran_at: u64, now: u64) -> Duration {
    let mut note = read_note(&inner.paths.data, plugin);
    if note.ran_at < ran_at {
        note.ran_at = ran_at;
        note.error = Some("its note was not kept".into());
    }
    if note.ran_at == 0 {
        return Duration::ZERO;
    }
    let every = plugin
        .manifest
        .publish
        .as_ref()
        .and_then(|publish| publish.every().ok())
        .unwrap_or(RETRY_WAIT);
    let wait = if note.error.is_some() {
        every.min(RETRY_WAIT)
    } else {
        every
    };
    Duration::from_secs((note.ran_at + wait.as_secs()).saturating_sub(now))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}
