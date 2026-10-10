//! Backups of what cannot be downloaded again: the settings, the node's
//! identity in the network and its crawl credits, and the remote control
//! files. Sites, the index and seed downloads are left out: a node rebuilds
//! them by itself.
//!
//! A backup is one JSON file, kept in `DIR/backups/` (readable by its owner
//! only, on Unix, since it holds keys) and downloadable from the panel.
//! Restoring writes back only the files named in [`FILES`], whatever else a
//! backup file says, after backing up the files it replaces. It writes them
//! again as the node next starts ([`apply_pending`]): the running network
//! keeps its keys and credits in memory, and saves them over the restored
//! files until it stops.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use plumb_core::now_unix;
use serde::{Deserialize, Serialize};

use super::control::{hex_decode, hex_encode};

/// The folder of backups in the data directory.
pub const DIR_NAME: &str = "backups";
/// Backups kept; older ones are deleted as new ones are made.
pub const KEEP: usize = 10;
/// A restore waiting for the next start, in the data directory.
pub const PENDING_FILE: &str = "restore-pending.json";
/// Where a pending restore that failed is moved, so it is not tried at
/// every start.
const FAILED_FILE: &str = "restore-failed.json";
/// What a backup file starts its format name with.
const FORMAT: &str = "plumb-backup/1";

/// The files a backup holds, relative to the data directory, with what
/// each is, for the panel.
pub const FILES: [(&str, &str); 9] = [
    ("settings.json", "Crawling and limit settings"),
    ("features.json", "Feature settings"),
    ("remote-control.json", "Remote control token"),
    ("remote-nodes.json", "Nodes this app controls"),
    ("net/node.key", "Network identity (node key)"),
    ("net/credits/token.key", "Credit issuing key"),
    ("net/credits/wallet.json", "Tokens held"),
    ("net/credits/ledger.json", "Crawl credit accounts"),
    ("net/credits/spent", "Spent tokens"),
];

/// A backup file's contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backup {
    pub format: String,
    /// Unix seconds.
    pub created_at: u64,
    /// The version of Plumb that made it.
    pub version: String,
    /// File contents, hex-encoded, by path relative to the data directory.
    pub files: BTreeMap<String, String>,
}

/// A backup in `DIR/backups/`, for the panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupInfo {
    pub name: String,
    pub created_at: u64,
    pub bytes: u64,
}

impl Backup {
    /// Reads the files of the node in `data`.
    pub fn make(data: &Path) -> Result<Backup> {
        let mut files = BTreeMap::new();
        for (name, _) in FILES {
            match std::fs::read(data.join(name)) {
                Ok(bytes) => {
                    files.insert(name.to_owned(), hex_encode(&bytes));
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err).with_context(|| format!("reading {name}")),
            }
        }
        Ok(Backup {
            format: FORMAT.to_owned(),
            created_at: now_unix(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            files,
        })
    }

    /// Reads a backup file's text, refusing anything else.
    pub fn parse(bytes: &[u8]) -> Result<Backup> {
        let backup: Backup =
            serde_json::from_slice(bytes).context("This is not a Plumb Search backup file")?;
        if backup.format != FORMAT {
            bail!(
                "This backup was made in another format ({}), which this version cannot read",
                backup.format
            );
        }
        for (name, hex) in &backup.files {
            if !FILES.iter().any(|(known, _)| known == name) {
                bail!("The backup holds a file Plumb does not restore: {name}");
            }
            hex_decode(hex).with_context(|| format!("The backup's copy of {name} is damaged"))?;
        }
        Ok(backup)
    }

    /// What the backup holds, in words, for the panel.
    pub fn contents(&self) -> Vec<&'static str> {
        FILES
            .iter()
            .filter(|(name, _)| self.files.contains_key(*name))
            .map(|(_, what)| *what)
            .collect()
    }

    /// Writes the backup's files into the data directory `data`. Files it
    /// does not hold are left as they are.
    pub fn restore(&self, data: &Path) -> Result<()> {
        for (name, _) in FILES {
            let Some(hex) = self.files.get(name) else {
                continue;
            };
            let bytes = hex_decode(hex)?;
            let path = data.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            write_private(&path, &bytes)?;
        }
        Ok(())
    }
}

impl Backup {
    /// Keeps the backup in `data` to be restored again as the node next
    /// starts, before anything reads the files ([`apply_pending`]).
    pub fn stage(&self, data: &Path) -> Result<()> {
        write_private(&data.join(PENDING_FILE), &serde_json::to_vec(self)?)
    }
}

/// Restores the backup [`Backup::stage`] kept in `data`, if any, and
/// removes it: `true` when one was restored. One that cannot be restored
/// is moved aside, not tried again.
pub fn apply_pending(data: &Path) -> Result<bool> {
    let path = data.join(PENDING_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let restored = Backup::parse(&bytes).and_then(|backup| backup.restore(data));
    if let Err(err) = restored {
        let _ = std::fs::rename(&path, data.join(FAILED_FILE));
        return Err(err.context("finishing the restore of a backup"));
    }
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    Ok(true)
}

/// Makes a backup of the node in `data` and saves it in `DIR/backups/`,
/// deleting the oldest beyond [`KEEP`]. `label` is put in the file name,
/// such as `before-restore`.
pub fn save(data: &Path, label: Option<&str>) -> Result<BackupInfo> {
    let backup = Backup::make(data)?;
    let dir = data.join(DIR_NAME);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let stamp = chrono::DateTime::from_timestamp(backup.created_at as i64, 0)
        .map(|t| t.format("%Y-%m-%d-%H%M%S").to_string())
        .unwrap_or_else(|| backup.created_at.to_string());
    let mut name = format!("plumb-backup-{stamp}");
    if let Some(label) = label {
        name.push('-');
        name.push_str(label);
    }
    // Two backups in the same second.
    let mut file = format!("{name}.json");
    let mut n = 2;
    while dir.join(&file).exists() {
        file = format!("{name}-{n}.json");
        n += 1;
    }
    let bytes = serde_json::to_vec_pretty(&backup)?;
    write_private(&dir.join(&file), &bytes)?;
    let mut all = list(data);
    while all.len() > KEEP {
        if let Some(oldest) = all.pop() {
            let _ = std::fs::remove_file(dir.join(oldest.name));
        }
    }
    // As list() describes it: the file's time can be a second past the
    // backup's own.
    let (_, info) =
        info_of(&dir.join(&file), file).with_context(|| format!("reading {}", dir.display()))?;
    Ok(info)
}

/// The backups in `DIR/backups/`, newest first.
pub fn list(data: &Path) -> Vec<BackupInfo> {
    let Ok(entries) = std::fs::read_dir(data.join(DIR_NAME)) else {
        return Vec::new();
    };
    let mut backups: Vec<(std::time::SystemTime, BackupInfo)> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !is_backup_name(&name) {
                return None;
            }
            info_of(&entry.path(), name)
        })
        .collect();
    // By the full modification time, as backups made in the same second
    // have names that do not sort by age (`-10` before `-2`, `-2` before
    // none).
    backups.sort_by(|(a_time, a), (b_time, b)| (b_time, &b.name).cmp(&(a_time, &a.name)));
    backups.into_iter().map(|(_, info)| info).collect()
}

/// When the backup file at `path` was written, and what the panel shows of it.
fn info_of(path: &Path, name: String) -> Option<(std::time::SystemTime, BackupInfo)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let created_at = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((
        modified,
        BackupInfo {
            name,
            created_at,
            bytes: meta.len(),
        },
    ))
}

/// The path of the backup named `name`, when that is a backup's name: no
/// other file can be read or restored through it.
pub fn path_of(data: &Path, name: &str) -> Option<PathBuf> {
    is_backup_name(name).then(|| data.join(DIR_NAME).join(name))
}

fn is_backup_name(name: &str) -> bool {
    name.starts_with("plumb-backup-")
        && name.ends_with(".json")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && !name.contains("..")
}

/// Writes `bytes` to `path` in one go, readable by its owner only.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let tmp = crate::temp_path_for(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let written = options.open(&tmp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    if let Err(err) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("writing {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backup_restores_settings_and_keys_and_nothing_else() {
        let from = tempfile::tempdir().unwrap();
        std::fs::write(
            from.path().join("settings.json"),
            "{\"workload\":\"light\"}",
        )
        .unwrap();
        std::fs::create_dir_all(from.path().join("net")).unwrap();
        std::fs::write(from.path().join("net/node.key"), [1u8, 2, 3, 255]).unwrap();
        std::fs::write(from.path().join("records.jsonl"), "{}").unwrap();
        let saved = save(from.path(), None).unwrap();
        assert_eq!(list(from.path())[0], saved);
        let bytes = std::fs::read(path_of(from.path(), &saved.name).unwrap()).unwrap();
        let backup = Backup::parse(&bytes).unwrap();
        assert_eq!(
            backup.contents(),
            ["Crawling and limit settings", "Network identity (node key)"]
        );

        let to = tempfile::tempdir().unwrap();
        backup.restore(to.path()).unwrap();
        assert_eq!(
            std::fs::read(to.path().join("net/node.key")).unwrap(),
            [1u8, 2, 3, 255]
        );
        assert!(to.path().join("settings.json").is_file());
        assert!(!to.path().join("records.jsonl").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(to.path().join("net/node.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_staged_backup_is_restored_once_at_the_next_start() {
        let from = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(from.path().join("net/credits")).unwrap();
        std::fs::write(from.path().join("net/credits/wallet.json"), b"old").unwrap();
        let backup = Backup::make(from.path()).unwrap();

        let to = tempfile::tempdir().unwrap();
        assert!(!apply_pending(to.path()).unwrap());
        backup.stage(to.path()).unwrap();
        backup.restore(to.path()).unwrap();
        // The running network saves its own wallet over it.
        std::fs::write(to.path().join("net/credits/wallet.json"), b"new").unwrap();
        assert!(apply_pending(to.path()).unwrap());
        assert_eq!(
            std::fs::read(to.path().join("net/credits/wallet.json")).unwrap(),
            b"old"
        );
        assert!(!to.path().join(PENDING_FILE).exists());
        assert!(!apply_pending(to.path()).unwrap());

        // A damaged one is moved aside, not tried at every start.
        std::fs::write(to.path().join(PENDING_FILE), b"not a backup").unwrap();
        assert!(apply_pending(to.path()).is_err());
        assert!(!to.path().join(PENDING_FILE).exists());
        assert!(to.path().join(FAILED_FILE).exists());
    }

    #[test]
    fn a_backup_naming_other_files_is_refused() {
        let mut backup = Backup::make(tempfile::tempdir().unwrap().path()).unwrap();
        backup
            .files
            .insert("../../.ssh/authorized_keys".into(), hex_encode(b"x"));
        let bytes = serde_json::to_vec(&backup).unwrap();
        assert!(Backup::parse(&bytes).is_err());
        assert!(Backup::parse(b"not json").is_err());
        assert!(path_of(Path::new("/d"), "../settings.json").is_none());
        assert!(path_of(Path::new("/d"), "plumb-backup-x/../../a.json").is_none());
        assert!(path_of(Path::new("/d"), "plumb-backup-2026-10-03-120000.json").is_some());
    }

    #[test]
    fn the_newest_backup_lists_first_and_as_saved() {
        let dir = tempfile::tempdir().unwrap();
        // Within a second or two, so some share a time stamp in their names.
        for _ in 0..12 {
            let saved = save(dir.path(), None).unwrap();
            assert_eq!(list(dir.path())[0], saved);
        }
    }

    #[test]
    fn only_the_newest_backups_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..KEEP + 3 {
            save(dir.path(), None).unwrap();
        }
        assert_eq!(list(dir.path()).len(), KEEP);
    }
}
