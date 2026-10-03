//! The popularity reports a node holds (see [`crate::popularity`]): this
//! week's and last week's, from every node, so it can count them itself
//! and hand them to nodes that were away.
//!
//! ```text
//! DIR/net/reports/<epoch>.jsonl   one report per line
//! ```

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::warn;

use crate::hash::Hash;
use crate::popularity::{report_epoch, tally, PopularityTable, Report, MAX_REPORTS_PER_EPOCH};

#[derive(Debug)]
pub struct ReportStore {
    dir: PathBuf,
    ids: HashSet<Hash>,
    by_epoch: BTreeMap<u64, Vec<Report>>,
}

impl ReportStore {
    /// Opens the store in `dir`, creating it, and reads the reports of
    /// the current and the previous week; older files are deleted.
    pub fn open(dir: &Path, now: u64) -> Result<ReportStore> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut store = ReportStore {
            dir: dir.to_path_buf(),
            ids: HashSet::new(),
            by_epoch: BTreeMap::new(),
        };
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            let epoch = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".jsonl"))
                .and_then(|n| n.parse::<u64>().ok());
            let Some(epoch) = epoch.filter(|&e| store.keeps(e, now)) else {
                let _ = fs::remove_file(&path);
                continue;
            };
            let file =
                fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
            for line in BufReader::new(file).lines() {
                let Ok(report) = serde_json::from_str::<Report>(&line?) else {
                    continue;
                };
                if report.epoch == epoch && store.ids.insert(report.id()) {
                    store.by_epoch.entry(epoch).or_default().push(report);
                }
            }
        }
        Ok(store)
    }

    fn keeps(&self, epoch: u64, now: u64) -> bool {
        let current = report_epoch(now);
        epoch <= current && epoch + 1 >= current
    }

    /// Reports held.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn contains(&self, id: &Hash) -> bool {
        self.ids.contains(id)
    }

    /// Keeps a report that passed [`Report::check`]. Returns whether it
    /// was new; a full epoch takes no more.
    pub fn insert(&mut self, report: &Report) -> Result<bool> {
        let id = report.id();
        if self.ids.contains(&id) {
            return Ok(false);
        }
        let held = self.by_epoch.get(&report.epoch).map_or(0, Vec::len);
        if held >= MAX_REPORTS_PER_EPOCH {
            return Ok(false);
        }
        let path = self.dir.join(format!("{}.jsonl", report.epoch));
        let mut line = serde_json::to_vec(report).context("encoding a report")?;
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.write_all(&line)
            .with_context(|| format!("writing {}", path.display()))?;
        self.ids.insert(id);
        self.by_epoch
            .entry(report.epoch)
            .or_default()
            .push(report.clone());
        Ok(true)
    }

    /// Up to `max` reports of `epoch`.
    pub fn list(&self, epoch: u64, max: usize) -> Vec<Report> {
        self.by_epoch
            .get(&epoch)
            .map(|reports| reports.iter().take(max).cloned().collect())
            .unwrap_or_default()
    }

    /// Drops the reports of weeks before last week.
    pub fn prune(&mut self, now: u64) {
        let old: Vec<u64> = self
            .by_epoch
            .keys()
            .copied()
            .filter(|&e| !self.keeps(e, now))
            .collect();
        for epoch in old {
            for report in self.by_epoch.remove(&epoch).unwrap_or_default() {
                self.ids.remove(&report.id());
            }
            let path = self.dir.join(format!("{epoch}.jsonl"));
            if let Err(err) = fs::remove_file(&path) {
                warn!("cannot delete {}: {err}", path.display());
            }
        }
    }

    /// Counts the reports of this week and last week.
    pub fn table(&self, now: u64) -> PopularityTable {
        let current = report_epoch(now);
        let epochs: Vec<u64> = [current.saturating_sub(1), current]
            .into_iter()
            .filter(|e| self.by_epoch.contains_key(e))
            .collect();
        let tallies = epochs
            .iter()
            .flat_map(|&e| tally(&self.by_epoch[&e], e))
            .collect::<Vec<_>>();
        PopularityTable::new(epochs, tallies)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::popularity::{REPORT_EPOCH_SECS, REPORT_THRESHOLD};

    const NOW: u64 = 1_790_000_000;

    #[test]
    fn keeps_two_weeks_of_reports_and_counts_them() {
        let dir = tempfile::tempdir().unwrap();
        let epoch = report_epoch(NOW);
        let mut store = ReportStore::open(dir.path(), NOW).unwrap();
        let report = |e| Report::new(e, "us bank", "usbank.com").unwrap();
        let first = report(epoch);
        assert!(store.insert(&first).unwrap());
        assert!(!store.insert(&first).unwrap());
        for _ in 1..REPORT_THRESHOLD {
            store.insert(&report(epoch)).unwrap();
        }
        store.insert(&report(epoch - 1)).unwrap();
        assert_eq!(store.len(), REPORT_THRESHOLD as usize + 1);
        assert_eq!(store.list(epoch, 3).len(), 3);

        let store = ReportStore::open(dir.path(), NOW).unwrap();
        assert_eq!(store.len(), REPORT_THRESHOLD as usize + 1);
        let table = store.table(NOW);
        assert_eq!(table.picks.len(), 1);
        assert_eq!(table.picks[0].count, REPORT_THRESHOLD);
        assert!(table.bonus("us bank", "usbank.com") > 0.0);

        // Two weeks on, all of it is gone.
        let later = NOW + 2 * REPORT_EPOCH_SECS;
        let mut store = store;
        store.prune(later);
        assert!(store.is_empty());
        assert!(ReportStore::open(dir.path(), later).unwrap().is_empty());
    }
}
