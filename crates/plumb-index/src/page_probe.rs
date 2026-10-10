//! Local-only exact title-key probe. No raw-corpus reads, IndexReader,
//! cache manager, writer, server, or global URL-absence claim.
//!
//! The caller must separately authorize and retain an immutable generation.
//! Metadata hashes and file identities are rechecked; this cannot lock a
//! production publisher/pruner or prove that a caller's retention claim is true.
//! RSS/I/O/time checks are cooperative. An external watchdog is required for
//! a production pilot because a single mmap fault or decompression can exceed
//! a threshold before the next check. Virtual mmap size is not resident memory.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Instant, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    Directory, DirectoryLock, FileHandle, Lock, MmapDirectory, WatchCallback, WatchHandle, WritePtr,
};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{DocSet, Index, SegmentReader, TantivyDocument, Term, TERMINATED};

use crate::analysis;
use crate::pages::{Page, PAGE_INDEX_VERSION};

pub const MAX_DOCUMENTS: usize = 20;
pub const MAX_SEGMENTS: usize = 32;
pub const MAX_TARGETS: usize = 5;
pub const MAX_TITLES: usize = 10;
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_METADATA_BYTES: usize = 1024 * 1024;
const MAX_PAGE_BYTES: usize = 1024 * 1024;
const MAX_WALL_MS: u128 = 15_000;
const MAX_RSS_BYTES: u64 = 128 * 1024 * 1024;
const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub generation: String,
    pub schema_version: String,
    pub meta_sha256: String,
    pub pages_sha256: String,
    pub managed_sha256: Option<String>,
    /// Digest of an independently approved small source-generation manifest.
    /// The probe records this attestation; it does not read/hash raw sources.
    pub source_manifest_sha256: String,
    pub runtime_commit: String,
    pub runtime_binary_sha256: String,
    pub diagnostic_source_commit: String,
    pub diagnostic_binary_sha256: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub expected_url: String,
    pub titles: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub binding: Binding,
    /// Must come from a separate explicit execution/retention authorization.
    pub immutable_retained: bool,
    pub targets: Vec<Target>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    InvalidRequest,
    BindingChanged,
    SchemaMismatch,
    MetadataCap,
    SegmentCap,
    DocumentCap,
    UnusableKey,
    ResourceStop,
    ReadFailed,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Claim {
    IndexedTargetPresent,
    KeyAbsent,
    CompleteKeyHitsWithoutTarget,
    Inconclusive,
}

#[derive(Serialize)]
pub struct Match {
    pub title: String,
    pub url: String,
}

#[derive(Serialize)]
pub struct TargetResult {
    pub expected_url: String,
    pub keys: Vec<String>,
    pub claim: Claim,
    pub matches: Vec<Match>,
}

#[derive(Default, Serialize)]
pub struct Counts {
    pub segments: usize,
    pub term_document_frequency: u64,
    pub postings_seen: usize,
    pub deleted_postings: usize,
    pub stored_documents_read: usize,
    pub elapsed_ms: u128,
    pub rss_bytes: Option<u64>,
    pub physical_read_bytes: Option<u64>,
}

#[derive(Serialize)]
pub struct Report {
    pub complete: bool,
    pub reason: Option<Reason>,
    /// Runtime/source attestations supplied by the authorized caller; the
    /// CLI independently checks its own binary/source binding.
    pub binding: Option<Binding>,
    pub opstamp: Option<u64>,
    pub segment_ids: Vec<String>,
    pub key_document_frequency: BTreeMap<String, u64>,
    pub counts: Counts,
    pub targets: Vec<TargetResult>,
    pub global_url_absence_proven: bool,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid(request: &Request) -> bool {
    let b = &request.binding;
    request.immutable_retained
        && b.schema_version == PAGE_INDEX_VERSION
        && !b.generation.is_empty()
        && b.generation.len() <= 64
        && b.generation
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        && hex(&b.runtime_commit, 40)
        && hex(&b.diagnostic_source_commit, 40)
        && [
            &b.meta_sha256,
            &b.pages_sha256,
            &b.source_manifest_sha256,
            &b.runtime_binary_sha256,
            &b.diagnostic_binary_sha256,
        ]
        .iter()
        .all(|value| hex(value, 64))
        && b.managed_sha256.as_ref().is_none_or(|value| hex(value, 64))
        && !request.targets.is_empty()
        && request.targets.len() <= MAX_TARGETS
        && request
            .targets
            .iter()
            .map(|target| &target.expected_url)
            .collect::<HashSet<_>>()
            .len()
            == request.targets.len()
        && request
            .targets
            .iter()
            .map(|t| t.titles.len())
            .sum::<usize>()
            <= MAX_TITLES
        && request.targets.iter().all(|target| {
            target.expected_url.starts_with("https://")
                && target.expected_url.len() <= 2048
                && !target.expected_url.contains('@')
                && !target.expected_url.chars().any(char::is_whitespace)
                && plumb_core::host_of(&target.expected_url).is_some()
                && !target.titles.is_empty()
                && target
                    .titles
                    .iter()
                    .all(|title| !title.is_empty() && title.len() <= 1024)
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64),
}

fn stamp(path: &Path) -> io::Result<Option<Stamp>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !(meta.is_file() || meta.is_dir()) || meta.file_type().is_symlink() {
        return Err(denied());
    }
    Ok(Some(Stamp {
        len: meta.len(),
        modified: meta.modified()?,
        #[cfg(unix)]
        identity: {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino())
        },
    }))
}

fn denied() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "read-only probe")
}

/// Every write/lock/watch operation fails. Allowed files are a bounded
/// metadata/segment catalog, with identities checked on each open.
#[derive(Clone, Debug)]
struct ReadOnlyDirectory {
    root: PathBuf,
    root_stamp: Stamp,
    inner: MmapDirectory,
    files: Arc<RwLock<BTreeMap<PathBuf, Option<Stamp>>>>,
}

impl ReadOnlyDirectory {
    fn open(root: &Path) -> Result<Self, Reason> {
        if !root.is_dir() {
            return Err(Reason::ReadFailed);
        }
        let root_stamp = stamp(root)
            .map_err(|_| Reason::ReadFailed)?
            .ok_or(Reason::ReadFailed)?;
        // Resolve parent links once, retaining the concrete directory. A
        // final-component symlink was rejected by stamp above.
        let root = fs::canonicalize(root).map_err(|_| Reason::ReadFailed)?;
        let mut files = BTreeMap::new();
        for name in ["meta.json", ".managed.json", "pages.json"] {
            let entry = stamp(&root.join(name)).map_err(|_| Reason::ReadFailed)?;
            let cap = if name == "pages.json" {
                MAX_REQUEST_BYTES
            } else {
                MAX_METADATA_BYTES
            };
            if entry.as_ref().is_some_and(|stamp| stamp.len > cap as u64) {
                return Err(Reason::MetadataCap);
            }
            files.insert(PathBuf::from(name), entry);
        }
        Ok(Self {
            root: root.clone(),
            root_stamp,
            inner: MmapDirectory::open(&root).map_err(|_| Reason::ReadFailed)?,
            files: Arc::new(RwLock::new(files)),
        })
    }

    fn check(&self, path: &Path) -> io::Result<bool> {
        let files = self.files.read().map_err(|_| denied())?;
        let expected = files.get(path).ok_or_else(denied)?;
        if stamp(&self.root.join(path))? != *expected {
            return Err(denied());
        }
        Ok(expected.is_some())
    }

    fn verify(&self) -> Result<(), Reason> {
        if stamp(&self.root).map_err(|_| Reason::ReadFailed)?.as_ref() != Some(&self.root_stamp) {
            return Err(Reason::BindingChanged);
        }
        let files = self.files.read().map_err(|_| Reason::ReadFailed)?;
        for (path, expected) in files.iter() {
            if stamp(&self.root.join(path)).map_err(|_| Reason::ReadFailed)? != *expected {
                return Err(Reason::BindingChanged);
            }
        }
        Ok(())
    }
}

impl Directory for ReadOnlyDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        match self
            .check(path)
            .map_err(|e| OpenReadError::wrap_io_error(e, path.to_owned()))?
        {
            true => self.inner.get_file_handle(path),
            false => Err(OpenReadError::FileDoesNotExist(path.to_owned())),
        }
    }
    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        self.check(path)
            .map_err(|e| OpenReadError::wrap_io_error(e, path.to_owned()))
    }
    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        let fail = |e| OpenReadError::wrap_io_error(e, path.to_owned());
        if !self.check(path).map_err(fail)? {
            return Err(OpenReadError::FileDoesNotExist(path.to_owned()));
        }
        let mut bytes = Vec::new();
        fs::File::open(self.root.join(path))
            .map_err(fail)?
            .take((MAX_METADATA_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(fail)?;
        if bytes.len() > MAX_METADATA_BYTES {
            return Err(fail(denied()));
        }
        self.check(path).map_err(fail)?;
        Ok(bytes)
    }
    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        Err(DeleteError::IoError {
            io_error: Arc::new(denied()),
            filepath: path.to_owned(),
        })
    }
    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        Err(OpenWriteError::wrap_io_error(denied(), path.to_owned()))
    }
    fn atomic_write(&self, _: &Path, _: &[u8]) -> io::Result<()> {
        Err(denied())
    }
    fn sync_directory(&self) -> io::Result<()> {
        Err(denied())
    }
    fn acquire_lock(&self, _: &Lock) -> Result<DirectoryLock, LockError> {
        Err(LockError::wrap_io_error(denied()))
    }
    fn watch(&self, _: WatchCallback) -> tantivy::Result<WatchHandle> {
        Err(tantivy::TantivyError::InvalidArgument(
            "read-only probe".into(),
        ))
    }
}

fn check_hashes(directory: &ReadOnlyDirectory, binding: &Binding) -> Result<(), Reason> {
    for (name, expected) in [
        ("meta.json", Some(&binding.meta_sha256)),
        ("pages.json", Some(&binding.pages_sha256)),
        (".managed.json", binding.managed_sha256.as_ref()),
    ] {
        let exists = directory
            .exists(Path::new(name))
            .map_err(|_| Reason::BindingChanged)?;
        match (exists, expected) {
            (false, None) => {}
            (true, Some(expected))
                if digest(
                    &directory
                        .atomic_read(Path::new(name))
                        .map_err(|_| Reason::ReadFailed)?,
                ) == *expected => {}
            _ => return Err(Reason::BindingChanged),
        }
    }
    directory.verify()
}

fn resource_sample() -> io::Result<(Option<u64>, Option<u64>)> {
    #[cfg(target_os = "linux")]
    {
        fn number(path: &str, key: &str) -> io::Result<u64> {
            let mut text = String::new();
            fs::File::open(path)?
                .take(64 * 1024)
                .read_to_string(&mut text)?;
            text.lines()
                .find_map(|line| {
                    let rest = line.strip_prefix(key)?;
                    rest.split_whitespace().next()?.parse().ok()
                })
                .ok_or_else(|| io::Error::other("missing process counter"))
        }
        Ok((
            Some(number("/proc/self/status", "VmRSS:")? * 1024),
            Some(number("/proc/self/io", "read_bytes:")?),
        ))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok((None, None))
    }
}

struct Guard {
    started: Instant,
    initial_reads: Option<u64>,
}
impl Guard {
    fn check(&self, counts: &mut Counts) -> Result<(), Reason> {
        counts.elapsed_ms = self.started.elapsed().as_millis();
        let (rss, reads) = resource_sample().map_err(|_| Reason::ResourceStop)?;
        counts.rss_bytes = rss;
        counts.physical_read_bytes = reads
            .zip(self.initial_reads)
            .map(|(now, before)| now.saturating_sub(before));
        if counts.elapsed_ms > MAX_WALL_MS
            || rss.is_some_and(|value| value > MAX_RSS_BYTES)
            || counts
                .physical_read_bytes
                .is_some_and(|value| value > MAX_READ_BYTES)
        {
            return Err(Reason::ResourceStop);
        }
        Ok(())
    }
}

/// Runs only on an explicitly authorized retained generation. A failure
/// clears all claims, including partially found targets, to inconclusive.
/// It never prints underlying errors or unwhitelisted stored contents.
pub fn run(root: &Path, request: &Request) -> Report {
    let valid_request = valid(request);
    let mut report = Report {
        complete: false,
        reason: None,
        binding: valid_request.then(|| request.binding.clone()),
        opstamp: None,
        segment_ids: Vec::new(),
        key_document_frequency: BTreeMap::new(),
        counts: Counts::default(),
        targets: if valid_request {
            request
                .targets
                .iter()
                .map(|target| TargetResult {
                    expected_url: target.expected_url.clone(),
                    keys: Vec::new(),
                    claim: Claim::Inconclusive,
                    matches: Vec::new(),
                })
                .collect()
        } else {
            Vec::new()
        },
        global_url_absence_proven: false,
    };
    match inspect(root, request, &mut report) {
        Ok(()) => report.complete = true,
        Err(reason) => {
            report.reason = Some(reason);
            for target in &mut report.targets {
                target.claim = Claim::Inconclusive;
                target.matches.clear();
            }
        }
    }
    report
}

fn inspect(root: &Path, request: &Request, report: &mut Report) -> Result<(), Reason> {
    let expected_directory = format!("index-{}", request.binding.generation);
    if !valid(request)
        || root.file_name().and_then(|n| n.to_str()) != Some(expected_directory.as_str())
    {
        return Err(Reason::InvalidRequest);
    }
    let guard = Guard {
        started: Instant::now(),
        initial_reads: resource_sample().map_err(|_| Reason::ResourceStop)?.1,
    };
    guard.check(&mut report.counts)?;
    let directory = ReadOnlyDirectory::open(root)?;
    check_hashes(&directory, &request.binding)?;
    let index = Index::open(directory.clone()).map_err(|_| Reason::ReadFailed)?;
    let meta = index.load_metas().map_err(|_| Reason::ReadFailed)?;
    if meta.schema != crate::pages::diagnostic_schema() {
        return Err(Reason::SchemaMismatch);
    }
    if meta.segments.len() > MAX_SEGMENTS {
        return Err(Reason::SegmentCap);
    }
    report.opstamp = Some(meta.opstamp);
    report.counts.segments = meta.segments.len();
    for segment in &meta.segments {
        report.segment_ids.push(segment.id().uuid_string());
        let mut files = directory.files.write().map_err(|_| Reason::ReadFailed)?;
        for path in segment.list_files() {
            let value = stamp(&directory.root.join(&path)).map_err(|_| Reason::ReadFailed)?;
            files.insert(path, value);
        }
    }
    check_hashes(&directory, &request.binding)?;
    let keys_field = meta
        .schema
        .get_field("keys")
        .map_err(|_| Reason::SchemaMismatch)?;
    let page_field = meta
        .schema
        .get_field("page")
        .map_err(|_| Reason::SchemaMismatch)?;
    let mut keys = BTreeMap::new();
    for (target, result) in request.targets.iter().zip(&mut report.targets) {
        for title in &target.titles {
            let key = analysis::tokens(&analysis::joined_analyzer(), title)
                .pop()
                .ok_or(Reason::UnusableKey)?;
            if !result.keys.contains(&key) {
                result.keys.push(key.clone());
            }
            keys.entry(key).or_insert(0u64);
        }
    }
    // Count exact-key postings before decoding any document. Includes
    // tombstones and overlap, so this conservative bound can stop early.
    for segment in &meta.segments {
        guard.check(&mut report.counts)?;
        let reader =
            SegmentReader::open(&index.segment(segment.clone())).map_err(|_| Reason::ReadFailed)?;
        let inverted = reader
            .inverted_index(keys_field)
            .map_err(|_| Reason::ReadFailed)?;
        for (key, frequency) in &mut keys {
            let term = Term::from_field_text(keys_field, key);
            if let Some(info) = inverted
                .get_term_info(&term)
                .map_err(|_| Reason::ReadFailed)?
            {
                *frequency += u64::from(info.doc_freq);
                report.counts.term_document_frequency += u64::from(info.doc_freq);
            }
        }
    }
    report.key_document_frequency = keys.clone();
    if report.counts.term_document_frequency > MAX_DOCUMENTS as u64 {
        return Err(Reason::DocumentCap);
    }
    let mut seen = HashSet::new();
    for (ordinal, segment) in meta.segments.iter().enumerate() {
        guard.check(&mut report.counts)?;
        let reader =
            SegmentReader::open(&index.segment(segment.clone())).map_err(|_| Reason::ReadFailed)?;
        let inverted = reader
            .inverted_index(keys_field)
            .map_err(|_| Reason::ReadFailed)?;
        let mut store = None;
        for (key, frequency) in &keys {
            if *frequency == 0 {
                continue;
            }
            let term = Term::from_field_text(keys_field, key);
            let Some(mut postings) = inverted
                .read_postings(&term, IndexRecordOption::Basic)
                .map_err(|_| Reason::ReadFailed)?
            else {
                continue;
            };
            while postings.doc() != TERMINATED {
                guard.check(&mut report.counts)?;
                report.counts.postings_seen += 1;
                if report.counts.postings_seen > MAX_DOCUMENTS {
                    return Err(Reason::DocumentCap);
                }
                let doc = postings.doc();
                if reader.is_deleted(doc) {
                    report.counts.deleted_postings += 1;
                } else if seen.insert((ordinal, doc)) {
                    if report.counts.stored_documents_read >= MAX_DOCUMENTS {
                        return Err(Reason::DocumentCap);
                    }
                    if store.is_none() {
                        store = Some(reader.get_store_reader(0).map_err(|_| Reason::ReadFailed)?);
                    }
                    let document: TantivyDocument = store
                        .as_ref()
                        .unwrap()
                        .get(doc)
                        .map_err(|_| Reason::ReadFailed)?;
                    report.counts.stored_documents_read += 1;
                    guard.check(&mut report.counts)?;
                    let raw = document
                        .get_first(page_field)
                        .and_then(|v| v.as_str())
                        .ok_or(Reason::ReadFailed)?;
                    if raw.len() > MAX_PAGE_BYTES {
                        return Err(Reason::ResourceStop);
                    }
                    let page: Page = serde_json::from_str(raw).map_err(|_| Reason::ReadFailed)?;
                    for target in &mut report.targets {
                        if page.url == target.expected_url {
                            if page.title.len() > 1024 {
                                return Err(Reason::ResourceStop);
                            }
                            target.matches.push(Match {
                                title: page.title.clone(),
                                url: page.url.clone(),
                            });
                        }
                    }
                }
                postings.advance();
            }
        }
    }
    guard.check(&mut report.counts)?;
    check_hashes(&directory, &request.binding)?;
    for target in &mut report.targets {
        target.claim = if !target.matches.is_empty() {
            Claim::IndexedTargetPresent
        } else if target.keys.iter().all(|key| keys[key] == 0) {
            Claim::KeyAbsent
        } else {
            Claim::CompleteKeyHitsWithoutTarget
        };
    }
    Ok(())
}

/// Hash only a bounded diagnostic executable, never index/corpus files.
pub fn executable_digest(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    if file.metadata()?.len() > MAX_READ_BYTES {
        return Err(denied());
    }
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::{build_page_index, Page};
    use plumb_core::article::Article;
    use tantivy::merge_policy::NoMergePolicy;

    fn public_page(title: &str, url: &str) -> Page {
        Page::from_reference(Article {
            title: title.into(),
            item: Some(url.into()),
            ..Article::default()
        })
        .unwrap()
    }

    fn fixture(pages: Vec<Page>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("index-synthetic");
        build_page_index(&root, pages).unwrap();
        (dir, root)
    }

    fn request(root: &Path, url: &str, title: &str) -> Request {
        let hash = |name: &str| digest(&fs::read(root.join(name)).unwrap());
        Request {
            immutable_retained: true,
            binding: Binding {
                generation: "synthetic".into(),
                schema_version: PAGE_INDEX_VERSION.into(),
                meta_sha256: hash("meta.json"),
                pages_sha256: hash("pages.json"),
                managed_sha256: root
                    .join(".managed.json")
                    .exists()
                    .then(|| hash(".managed.json")),
                source_manifest_sha256: "0".repeat(64),
                runtime_commit: "1".repeat(40),
                runtime_binary_sha256: "2".repeat(64),
                diagnostic_source_commit: "3".repeat(40),
                diagnostic_binary_sha256: "4".repeat(64),
            },
            targets: vec![Target {
                expected_url: url.into(),
                titles: vec![title.into()],
            }],
        }
    }

    // Only tiny fixture directories are enumerated here, never by the probe.
    fn fingerprint(root: &Path) -> BTreeMap<String, String> {
        fs::read_dir(root)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_str().unwrap().into(),
                    digest(&fs::read(entry.path()).unwrap()),
                )
            })
            .collect()
    }

    #[test]
    fn exact_joined_lookup_emits_only_whitelisted_matches_and_never_global_absence() {
        let url = "https://transit.example/guide";
        let page = public_page("Néstlé Transit", url);
        let mut private = public_page(
            "Néstlé Transit (PRIVATE_SENTINEL)",
            "https://private.example/secret",
        );
        private.description = Some("PRIVATE_BODY_SENTINEL".into());
        let (_dir, root) = fixture(vec![page, private]);
        let before = fingerprint(&root);
        let report = run(&root, &request(&root, url, "nestle  transit"));
        assert!(report.complete, "{:?}", report.reason);
        assert_eq!(report.targets[0].claim, Claim::IndexedTargetPresent);
        assert_eq!(report.counts.stored_documents_read, 2);
        let output = serde_json::to_string(&report).unwrap();
        assert!(!output.contains("PRIVATE") && !output.contains("private.example"));
        assert!(!report.global_url_absence_proven);
        assert_eq!(before, fingerprint(&root));
        let missing = run(&root, &request(&root, url, "Unseen title"));
        assert!(missing.complete);
        assert_eq!(missing.targets[0].claim, Claim::KeyAbsent);
        assert_eq!(missing.counts.stored_documents_read, 0);
        assert!(!missing.global_url_absence_proven);
    }

    #[test]
    fn words_only_alias_and_docs_title_are_keyspace_negatives_not_url_absence() {
        let mut article = Page::from_article(
            "en",
            Article {
                title: "Named Station".into(),
                aliases: vec!["Short Alias".into()],
                ..Article::default()
            },
        );
        article.url = "https://en.wikipedia.org/wiki/Named_Station".into();
        let docs = Page::from_docs(Article {
            title: "Introduction".into(),
            aliases: vec!["Python Introduction".into()],
            item: Some("https://docs.python.org/3/tutorial/index.html".into()),
            ..Article::default()
        })
        .unwrap();
        let (_dir, root) = fixture(vec![article.clone(), docs.clone()]);
        for (page, title) in [(&article, "Short Alias"), (&docs, "Introduction")] {
            let report = run(&root, &request(&root, &page.url, title));
            assert!(report.complete);
            assert_eq!(report.targets[0].claim, Claim::KeyAbsent);
            assert!(!report.global_url_absence_proven);
        }
        let report = run(&root, &request(&root, &docs.url, "python introduction"));
        assert!(report.complete);
        assert_eq!(report.targets[0].claim, Claim::IndexedTargetPresent);
    }

    #[test]
    fn empty_keys_complete_but_over_cap_postings_are_inconclusive_before_store_reads() {
        let (_dir, empty) = fixture(Vec::new());
        let report = run(
            &empty,
            &request(&empty, "https://public.example/page", "Zero"),
        );
        assert!(report.complete);
        assert_eq!(report.targets[0].claim, Claim::KeyAbsent);
        let pages = (0..MAX_DOCUMENTS + 1)
            .map(|i| public_page("Repeated title", &format!("https://public.example/{i}")))
            .collect();
        let (_dir, root) = fixture(pages);
        let report = run(
            &root,
            &request(&root, "https://public.example/0", "Repeated title"),
        );
        assert_eq!(report.reason, Some(Reason::DocumentCap));
        assert_eq!(report.targets[0].claim, Claim::Inconclusive);
        assert_eq!(report.counts.stored_documents_read, 0);
        assert!(!report.global_url_absence_proven);
    }

    #[test]
    fn tombstoned_store_rows_are_not_live_indexed_presence() {
        let url = "https://public.example/deleted";
        let (_dir, root) = fixture(vec![
            public_page("Delete me", url),
            public_page("Keep me", "https://public.example/keep"),
        ]);
        let index = Index::open_in_dir(&root).unwrap();
        let field = index.schema().get_field("keys").unwrap();
        let mut writer: tantivy::IndexWriter<TantivyDocument> =
            index.writer_with_num_threads(1, 15_000_000).unwrap();
        writer.set_merge_policy(Box::new(NoMergePolicy));
        writer.delete_term(Term::from_field_text(field, "deleteme"));
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();
        drop(index);
        let report = run(&root, &request(&root, url, "Delete me"));
        assert!(report.complete, "{:?}", report.reason);
        assert_eq!(report.targets[0].claim, Claim::CompleteKeyHitsWithoutTarget);
        assert_eq!(report.counts.deleted_postings, 1);
        assert_eq!(report.counts.stored_documents_read, 0);
    }

    #[test]
    fn read_only_directory_denies_mutations_locks_and_changed_bindings() {
        let url = "https://public.example/page";
        let (_dir, root) = fixture(vec![public_page("Public title", url)]);
        let before = fingerprint(&root);
        let directory = ReadOnlyDirectory::open(&root).unwrap();
        assert!(directory.open_write(Path::new("new-file")).is_err());
        assert!(directory
            .atomic_write(Path::new("meta.json"), b"bad")
            .is_err());
        assert!(directory.delete(Path::new("meta.json")).is_err());
        assert!(directory.sync_directory().is_err());
        assert!(directory
            .acquire_lock(&tantivy::directory::META_LOCK)
            .is_err());
        let index = Index::open(directory.clone()).unwrap();
        assert!(index.reader().is_err());
        assert_eq!(before, fingerprint(&root));
        let request = request(&root, url, "Public title");
        fs::write(root.join("pages.json"), b"{}").unwrap();
        assert_eq!(directory.verify(), Err(Reason::BindingChanged));
        let report = run(&root, &request);
        assert_eq!(report.reason, Some(Reason::BindingChanged));
        assert_eq!(report.targets[0].claim, Claim::Inconclusive);
        assert!(report.targets[0].matches.is_empty());
    }

    #[test]
    fn missing_retention_or_dropped_long_keys_fail_closed() {
        let url = "https://public.example/page";
        let (_dir, root) = fixture(vec![public_page("Public title", url)]);
        let mut req = request(&root, url, "Public title");
        req.immutable_retained = false;
        assert_eq!(run(&root, &req).reason, Some(Reason::InvalidRequest));
        req.immutable_retained = true;
        req.targets[0].titles = vec!["x".repeat(300)];
        assert_eq!(run(&root, &req).reason, Some(Reason::UnusableKey));
    }
}
