//! Downloading one file to disk, through a `.part` file renamed when
//! complete. A download that breaks off is carried on from where it stopped
//! when the server allows it.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::header::{ACCEPT_RANGES, CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE};
use reqwest::StatusCode;
use serde::Deserialize;
use tracing::{info, warn};

/// Progress is logged about this often when the download size is unknown.
const PROGRESS_BYTES_UNKNOWN_LENGTH: u64 = 64 << 20;

/// Times a download that broke off is carried on within one call to
/// [`download_to_file`].
const MAX_RESUMES: u32 = 5;
/// Pause before carrying on a download that broke off.
const RESUME_PAUSE: Duration = Duration::from_secs(1);

/// Streams `url` into `dest` (via a `.part` file renamed when complete),
/// logging progress. Fails on non-2xx responses. Returns bytes written.
///
/// Creates `dest`'s parent directory and replaces an existing `dest`. When
/// the server takes byte ranges and names the file's version (a strong
/// `ETag`, or `Last-Modified`), a download that breaks off is carried on
/// from where it stopped: up to [`MAX_RESUMES`] times in this call, and
/// from the `.part` file it leaves (with a `.part.json` beside it naming
/// the version) by a later call for the same URL, unless the file has
/// changed since. Otherwise the `.part` file is removed when the download
/// fails.
pub async fn download_to_file(client: &reqwest::Client, url: &str, dest: &Path) -> Result<u64> {
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        crate::storage::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let part = part_path(dest);
    let info = part_info_path(dest);
    let mut resumes = 0;
    let written = loop {
        let from = PartInfo::resumable(&part, &info, url);
        match download_attempt(client, url, dest, &part, &info, from).await {
            Ok(written) => break written,
            Err(failed) if failed.resumable && resumes < MAX_RESUMES => {
                resumes += 1;
                warn!("downloading {url}: {:#}; carrying on", failed.error);
                tokio::time::sleep(RESUME_PAUSE).await;
            }
            Err(failed) => {
                if !failed.resumable {
                    let _ = crate::storage::remove_file(&part).await;
                    let _ = crate::storage::remove_file(&info).await;
                }
                return Err(failed.error.context(format!("downloading {url}")));
            }
        }
    };
    let _ = crate::storage::remove_file(&info).await;
    crate::storage::rename(&part, dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    info!("saved {} ({})", dest.display(), megabytes(written));
    Ok(written)
}

/// Why one try at a download failed, and whether its `.part` file can be
/// carried on from.
struct AttemptFailed {
    error: anyhow::Error,
    resumable: bool,
}

impl AttemptFailed {
    fn new(error: anyhow::Error, resumable: bool) -> Self {
        AttemptFailed { error, resumable }
    }
}

/// What is saved beside a `.part` file that can be carried on from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
struct PartInfo {
    url: String,
    /// The file's version: a strong `ETag`, else its `Last-Modified`. Sent
    /// as `If-Range`, so a changed file comes whole.
    version: String,
    /// The whole file's size, when the server said.
    total: Option<u64>,
}

impl PartInfo {
    /// The bytes in `part` and what `info` says of them, when they are of
    /// `url` and can be carried on from.
    fn resumable(part: &Path, info: &Path, url: &str) -> Option<(u64, PartInfo)> {
        let saved: PartInfo = serde_json::from_slice(&std::fs::read(info).ok()?).ok()?;
        let have = std::fs::metadata(part).ok()?.len();
        let fits = saved.total.is_none_or(|total| have < total);
        (saved.url == url && have > 0 && fits).then_some((have, saved))
    }

    /// The version of the file a response holds, when the server takes
    /// byte ranges of it.
    fn of(url: &str, response: &reqwest::Response) -> Option<PartInfo> {
        let headers = response.headers();
        let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
        if !header(ACCEPT_RANGES).is_some_and(|v| v.trim().eq_ignore_ascii_case("bytes")) {
            return None;
        }
        let version = header(ETAG)
            .filter(|etag| !etag.starts_with("W/"))
            .or_else(|| header(LAST_MODIFIED))?;
        Some(PartInfo {
            url: url.to_string(),
            version: version.to_string(),
            total: response.content_length(),
        })
    }
}

/// `dest` with `.part.json` appended to its file name.
fn part_info_path(dest: &Path) -> PathBuf {
    let mut info = OsString::from(dest.as_os_str());
    info.push(".part.json");
    PathBuf::from(info)
}

/// The first byte and whole length in a `Content-Range` of
/// `bytes <first>-<last>/<length or *>`.
fn content_range(response: &reqwest::Response) -> Option<(u64, Option<u64>)> {
    let value = response.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    let (range, length) = value.strip_prefix("bytes ")?.split_once('/')?;
    let first = range.split_once('-')?.0.trim().parse().ok()?;
    Some((first, length.trim().parse().ok()))
}

/// One request for `url` into `part`, carrying on `from` the bytes already
/// there when the server agrees. Returns the file's whole size.
async fn download_attempt(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    part: &Path,
    info_path: &Path,
    from: Option<(u64, PartInfo)>,
) -> Result<u64, AttemptFailed> {
    let mut request = client.get(url);
    if let Some((have, info)) = &from {
        request = request
            .header(RANGE, format!("bytes={have}-"))
            .header(IF_RANGE, info.version.as_str());
    }
    // A part file kept from before is kept through a request that fails.
    let kept = from.is_some();
    let mut response = request
        .send()
        .await
        .with_context(|| format!("requesting {url}"))
        .map_err(|err| AttemptFailed::new(err, kept))?;
    let status = response.status();
    if !status.is_success() {
        let transient = status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS;
        let err = anyhow!("downloading {url} failed: HTTP {status}");
        return Err(AttemptFailed::new(err, kept && transient));
    }
    let carried = match (&from, status) {
        (Some((have, info)), StatusCode::PARTIAL_CONTENT) => match content_range(&response) {
            Some((first, total)) if first == *have => Some((*have, total.or(info.total))),
            _ => {
                let err = anyhow!("the server sent another range than the one asked for");
                return Err(AttemptFailed::new(err, false));
            }
        },
        (None, StatusCode::PARTIAL_CONTENT) => {
            let err = anyhow!("the server sent part of the file when asked for all of it");
            return Err(AttemptFailed::new(err, false));
        }
        _ => None,
    };
    let (start, total) = match carried {
        Some((have, total)) => {
            info!(
                "carrying on with {url} from {} ({})",
                megabytes(have),
                total.map_or_else(|| "size unknown".to_string(), megabytes)
            );
            (have, total)
        }
        None => {
            // The whole file: a fresh start, or a file that has changed.
            let total = response.content_length();
            info!(
                "downloading {url} to {} ({})",
                dest.display(),
                total.map_or_else(|| "size unknown".to_string(), megabytes)
            );
            let saved = match PartInfo::of(url, &response) {
                Some(info) => {
                    let bytes = serde_json::to_vec(&info).map_err(anyhow::Error::from);
                    match bytes {
                        Ok(bytes) => crate::storage::write(info_path, bytes).await.is_ok(),
                        Err(_) => false,
                    }
                }
                None => false,
            };
            if !saved {
                let _ = crate::storage::remove_file(info_path).await;
            }
            (0, total)
        }
    };
    let resumable = tokio::fs::try_exists(info_path).await.unwrap_or(false);
    stream_body(&mut response, part, start, total)
        .await
        .map_err(|err| AttemptFailed::new(err, resumable))
}

/// Writes the response body to `part` after its first `start` bytes,
/// returning the file's length then.
async fn stream_body(
    response: &mut reqwest::Response,
    part: &Path,
    start: u64,
    total: Option<u64>,
) -> Result<u64> {
    let mut file = crate::storage::OutputFile::open(part, start == 0)
        .await
        .with_context(|| format!("creating {}", part.display()))?;
    if start > 0 {
        file.seek(io::SeekFrom::Start(start))
            .await
            .with_context(|| format!("writing {}", part.display()))?;
    }
    let mut progress = Progress::new(total);
    let mut written = start;
    while let Some(chunk) = response.chunk().await.context("reading the response")? {
        file.write_all(&chunk)
            .await
            .with_context(|| format!("writing {}", part.display()))?;
        written += chunk.len() as u64;
        if progress.should_report(written) {
            match total {
                Some(total) => info!(
                    "{}: {}% ({} of {})",
                    part.display(),
                    written * 100 / total.max(1),
                    megabytes(written),
                    megabytes(total)
                ),
                None => info!("{}: {}", part.display(), megabytes(written)),
            }
        }
    }
    file.flush()
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    file.sync_all()
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    if let Some(total) = total {
        if written != total {
            bail!("expected {total} bytes but received {written}");
        }
    }
    Ok(written)
}

/// `dest` with `.part` appended to its file name.
pub fn part_path(dest: &Path) -> PathBuf {
    let mut part = OsString::from(dest.as_os_str());
    part.push(".part");
    PathBuf::from(part)
}

pub(super) fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// Decides when to log download progress: about every 5% of a known
/// length, or every 64 MB when the length is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Progress {
    step: u64,
    next: u64,
}

impl Progress {
    pub(super) fn new(total: Option<u64>) -> Self {
        let step = match total {
            Some(total) if total > 0 => (total / 20).max(1),
            _ => PROGRESS_BYTES_UNKNOWN_LENGTH,
        };
        Progress { step, next: step }
    }

    /// True when `written` bytes reach the next reporting point.
    pub(super) fn should_report(&mut self, written: u64) -> bool {
        if written < self.next {
            return false;
        }
        self.next = (written / self.step + 1) * self.step;
        true
    }
}

#[cfg(test)]
mod tests;
