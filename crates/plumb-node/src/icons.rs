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

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use plumb_core::SiteRecord;
use plumb_crawl::{normalize_icon, CrawlOutcome, CrawlResult};

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

/// Longest icon shared with a crawl, as base64 text: a 32-pixel PNG is
/// usually 1 to 3 KB, and the whole record must stay within
/// [`plumb_net::batch::MAX_RECORD_BYTES`].
const MAX_SHARED_ICON_CHARS: usize = 8 * 1024;

/// Puts each fetched homepage's icon on its record, for sharing a crawl
/// with the network (see [`SiteRecord::icon`]).
pub(crate) fn attach(records: &mut [SiteRecord], results: &[CrawlResult]) {
    let icons: HashMap<&str, &[u8]> = results
        .iter()
        .filter_map(|result| match &result.outcome {
            CrawlOutcome::Fetched(page) => Some((page.domain.as_str(), page.icon.as_deref()?)),
            _ => None,
        })
        .collect();
    for record in records {
        if let Some(icon) = icons.get(record.domain.as_str()) {
            let text = BASE64.encode(icon);
            if text.len() <= MAX_SHARED_ICON_CHARS {
                record.icon = Some(text);
            }
        }
    }
}

/// The icon another node shared, redrawn here ([`normalize_icon`]) so
/// what is kept is always a small PNG this node made. `None` when it is
/// not one.
pub(crate) fn from_shared(text: &str) -> Option<Vec<u8>> {
    if text.len() > MAX_SHARED_ICON_CHARS {
        return None;
    }
    normalize_icon(&BASE64.decode(text).ok()?)
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
pub(crate) mod tests {
    use super::*;

    /// An 8 by 8 red BMP, the smallest image an icon can be made from.
    pub(crate) fn bmp() -> Vec<u8> {
        let (side, row) = (8u32, 24u32);
        let size = 54 + row * side;
        let mut out = b"BM".to_vec();
        for v in [size, 0, 54, 40, side, side] {
            out.extend(v.to_le_bytes());
        }
        out.extend(1u16.to_le_bytes());
        out.extend(24u16.to_le_bytes());
        for v in [0u32, row * side, 2835, 2835, 0, 0] {
            out.extend(v.to_le_bytes());
        }
        for _ in 0..side * side {
            out.extend([0, 0, 255]);
        }
        out
    }

    #[test]
    fn icons_are_shared_as_base64_and_redrawn_when_taken_in() {
        let icon = normalize_icon(&bmp()).expect("an icon");
        let page = |domain: &str, icon: Option<Vec<u8>>| CrawlResult {
            domain: domain.into(),
            outcome: CrawlOutcome::Fetched(plumb_crawl::CrawledPage {
                domain: domain.into(),
                final_url: format!("https://{domain}/"),
                status: 200,
                fetched_at: 1,
                meta: plumb_crawl::PageMeta::default(),
                icon,
            }),
        };
        let mut records = vec![SiteRecord::new("a.com"), SiteRecord::new("b.com")];
        attach(
            &mut records,
            &[page("a.com", Some(icon.clone())), page("b.com", None)],
        );
        let shared = records[0].icon.as_deref().expect("shared");
        assert!(records[1].icon.is_none());
        let taken = from_shared(shared).expect("taken in");
        assert!(taken.starts_with(b"\x89PNG"));
        assert_eq!(from_shared("not base64!"), None);
        assert_eq!(from_shared(&BASE64.encode(b"not an image")), None);
        assert_eq!(from_shared(&"A".repeat(MAX_SHARED_ICON_CHARS + 4)), None);
    }

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
