//! Site icons a node keeps for its results pages.
//!
//! The crawler fetches each site's icon along with its homepage and
//! redraws it as a small PNG ([`plumb_crawl::normalize_icon`]). A node
//! keeps one file per site under `icons/` in its data folder and puts the
//! icon right into the results page, so a visitor's browser never asks a
//! site, or anyone but this node, for an icon: that would tell them what
//! was searched for.
//!
//! An empty file means the site was crawled and had no icon the crawler
//! could read, so it is not looked for again until the next crawl.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

/// Icon files larger than this are not shown.
const MAX_ICON_BYTES: u64 = 16 * 1024;

/// The icons of one data folder.
#[derive(Debug, Clone)]
pub struct IconStore {
    dir: PathBuf,
}

impl IconStore {
    /// The icons kept in `dir`, which is made on the first [`IconStore::put`].
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        IconStore { dir: dir.into() }
    }

    /// `dir/3f/example.com.png`: spread over 256 folders so none grows huge.
    /// `None` for a name that is not a plain lowercase host name.
    fn path(&self, domain: &str) -> Option<PathBuf> {
        let plain = !domain.is_empty()
            && domain.len() <= 253
            && !domain.starts_with('.')
            && domain
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
        if !plain || domain.contains("..") {
            return None;
        }
        Some(
            self.dir
                .join(format!("{:02x}", shard(domain)))
                .join(format!("{domain}.png")),
        )
    }

    /// The icon of `domain`, when there is one.
    pub fn get(&self, domain: &str) -> Option<Vec<u8>> {
        let path = self.path(domain)?;
        let size = fs::metadata(&path).ok()?.len();
        if size == 0 || size > MAX_ICON_BYTES {
            return None;
        }
        fs::read(path).ok()
    }

    /// Notes what a crawl of `domain` found: its icon, or that it had none.
    pub fn put(&self, domain: &str, icon: Option<&[u8]>) -> io::Result<()> {
        let Some(path) = self.path(domain) else {
            return Ok(());
        };
        let icon = icon.filter(|icon| icon.len() as u64 <= MAX_ICON_BYTES);
        let folder = path.parent().unwrap_or(&self.dir);
        fs::create_dir_all(folder)?;
        let temporary = folder.join(format!(".{domain}.tmp"));
        let mut file = fs::File::create(&temporary)?;
        file.write_all(icon.unwrap_or_default())?;
        drop(file);
        fs::rename(&temporary, &path)
    }

    /// Every site a crawl has noted, with or without an icon.
    pub fn noted(&self) -> HashSet<String> {
        let mut domains = HashSet::new();
        let Ok(shards) = fs::read_dir(&self.dir) else {
            return domains;
        };
        for shard in shards.flatten() {
            let Ok(files) = fs::read_dir(shard.path()) else {
                continue;
            };
            for file in files.flatten() {
                let name = file.file_name();
                let Some(domain) = name.to_str().and_then(|name| name.strip_suffix(".png")) else {
                    continue;
                };
                if !domain.starts_with('.') {
                    domains.insert(domain.to_string());
                }
            }
        }
        domains
    }
}

/// FNV-1a of `domain`, folded to a byte: stable across builds and machines.
fn shard(domain: &str) -> u8 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in domain.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    (hash ^ (hash >> 8) ^ (hash >> 16) ^ (hash >> 24)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_icons_and_notes_sites_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = IconStore::new(dir.path().join("icons"));
        assert_eq!(store.get("example.com"), None);
        assert!(store.noted().is_empty());

        store.put("example.com", Some(b"png bytes")).unwrap();
        store.put("plain.org", None).unwrap();
        assert_eq!(store.get("example.com").as_deref(), Some(&b"png bytes"[..]));
        assert_eq!(store.get("plain.org"), None);
        let noted = store.noted();
        assert!(noted.contains("example.com") && noted.contains("plain.org"));
        assert_eq!(noted.len(), 2);

        store.put("example.com", None).unwrap();
        assert_eq!(
            store.get("example.com"),
            None,
            "a new crawl replaces the icon"
        );
    }

    #[test]
    fn only_plain_host_names_become_paths() {
        let store = IconStore::new("/data/icons");
        for bad in [
            "",
            "../etc",
            "a/b.com",
            "EXAMPLE.com",
            ".hidden",
            "a..b",
            "x\0y",
        ] {
            assert_eq!(store.path(bad), None, "{bad:?}");
            assert!(store.put(bad, Some(b"x")).is_ok());
        }
        let path = store.path("www.example-1.co.uk").unwrap();
        assert!(path.starts_with("/data/icons"));
        assert!(path.ends_with("www.example-1.co.uk.png"));
    }

    #[test]
    fn oversized_icons_are_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = IconStore::new(dir.path());
        store
            .put("big.com", Some(&vec![1; MAX_ICON_BYTES as usize + 1]))
            .unwrap();
        assert_eq!(store.get("big.com"), None);
        assert!(store.noted().contains("big.com"));
    }
}
