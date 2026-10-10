//! The node's activity log: what it did and what went wrong, in sentences
//! for the person running it rather than for a developer. The panel shows
//! it; `DIR/activity.jsonl` keeps it across restarts.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use plumb_core::now_unix;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// The log's file in the data directory.
pub const FILE_NAME: &str = "activity.jsonl";
/// Entries kept, in memory and (after trimming) on disk.
pub const KEEP: usize = 300;
/// The file is trimmed back to [`KEEP`] entries once it holds this many.
const TRIM_AT: usize = 3 * KEEP;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warning,
    Error,
}

/// One line of the activity log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Unix seconds.
    pub at: u64,
    pub level: LogLevel,
    pub message: String,
}

/// The log, newest last.
#[derive(Debug)]
pub struct Journal {
    path: Option<PathBuf>,
    inner: Mutex<State>,
    budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
}

#[derive(Debug, Default)]
struct State {
    entries: VecDeque<LogEntry>,
    /// Lines in the file, to know when to trim it.
    lines: usize,
}

impl Journal {
    /// Opens the log of the node in `dir`, reading what earlier runs wrote.
    pub fn open(dir: &Path) -> Journal {
        let path = dir.join(FILE_NAME);
        let mut state = State::default();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                state.lines += 1;
                if let Ok(entry) = serde_json::from_str::<LogEntry>(line) {
                    state.entries.push_back(entry);
                    if state.entries.len() > KEEP {
                        state.entries.pop_front();
                    }
                }
            }
        }
        Journal {
            path: Some(path),
            budget: None,
            inner: Mutex::new(state),
        }
    }

    /// A log kept only in memory.
    pub fn in_memory() -> Journal {
        Journal {
            path: None,
            budget: None,
            inner: Mutex::new(State::default()),
        }
    }

    pub fn with_budget(
        mut self,
        budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
    ) -> Self {
        self.budget = budget;
        self
    }

    pub fn info(&self, message: impl Into<String>) {
        self.add(LogLevel::Info, message.into());
    }

    pub fn warning(&self, message: impl Into<String>) {
        self.add(LogLevel::Warning, message.into());
    }

    /// A warning that may come up every hour while nothing changes: kept
    /// once a day, so it does not push everything else out of the log.
    pub fn warning_daily(&self, message: impl Into<String>) {
        let message = message.into();
        let since = now_unix().saturating_sub(24 * 60 * 60);
        let said = self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .iter()
            .any(|e| e.at >= since && e.message == message);
        if !said {
            self.warning(message);
        }
    }

    pub fn error(&self, message: impl Into<String>) {
        self.add(LogLevel::Error, message.into());
    }

    fn add(&self, level: LogLevel, message: String) {
        let entry = LogEntry {
            at: now_unix(),
            level,
            message,
        };
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.entries.push_back(entry.clone());
        if state.entries.len() > KEEP {
            state.entries.pop_front();
        }
        let Some(path) = &self.path else {
            return;
        };
        let written = if state.lines + 1 >= TRIM_AT {
            // Rewrite the file with what is kept.
            let text: String = state
                .entries
                .iter()
                .filter_map(|e| serde_json::to_string(e).ok())
                .map(|line| line + "\n")
                .collect();
            state.lines = state.entries.len();
            if let Some(budget) = &self.budget {
                plumb_core::storage::write_atomic(
                    path,
                    &path.with_extension("quota.tmp"),
                    text.as_bytes(),
                    Some(budget),
                    None,
                )
                .map_err(Into::into)
            } else {
                super::store::write_atomically(path, text.as_bytes())
            }
        } else {
            state.lines += 1;
            let line = serde_json::to_string(&entry).expect("entries encode") + "\n";
            if let Some(budget) = &self.budget {
                plumb_core::storage::BudgetFile::open_write(path, false, Some(budget.clone()))
                    .and_then(|mut file| {
                        file.seek(std::io::SeekFrom::End(0))?;
                        file.write_all(line.as_bytes())
                    })
                    .map_err(Into::into)
            } else {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .and_then(|mut file| file.write_all(line.as_bytes()))
                    .map_err(Into::into)
            }
        };
        if let Err(err) = written {
            warn!("cannot write the activity log: {err:#}");
        }
    }

    /// The entries, newest first.
    pub fn entries(&self) -> Vec<LogEntry> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .iter()
            .rev()
            .cloned()
            .collect()
    }

    /// The last entry, if any.
    pub fn last(&self) -> Option<LogEntry> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .back()
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_warnings_are_kept_once_a_day() {
        let journal = Journal::in_memory();
        journal.warning_daily("Over the limit");
        journal.info("Crawled 10 sites");
        journal.warning_daily("Over the limit");
        journal.warning_daily("Something else");
        let messages: Vec<String> = journal.entries().into_iter().map(|e| e.message).collect();
        assert_eq!(
            messages,
            ["Something else", "Crawled 10 sites", "Over the limit"]
        );
    }

    #[test]
    fn the_log_survives_a_restart_and_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let log = Journal::open(dir.path());
        log.info("Started");
        log.error("Could not download the Tranco list");
        let log = Journal::open(dir.path());
        let entries = log.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].level, LogLevel::Error);
        assert_eq!(entries[1].message, "Started");
        for i in 0..TRIM_AT {
            log.info(format!("entry {i}"));
        }
        assert_eq!(log.entries().len(), KEEP);
        assert_eq!(log.entries()[0].message, format!("entry {}", TRIM_AT - 1));
        // The file was trimmed rather than growing without end.
        let lines = std::fs::read_to_string(dir.path().join(FILE_NAME))
            .unwrap()
            .lines()
            .count();
        assert!(lines < TRIM_AT, "{lines}");
        assert_eq!(Journal::open(dir.path()).entries(), log.entries());
    }
}
