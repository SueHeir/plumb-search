//! Immutable inner-page generations with a legacy filename adapter.
//! Build and validate before changing the advertised file; metadata is last.
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use plumb_core::Article;
use plumb_crawl::SitePagesTarget;
use plumb_net::pages::{quality_path, QualityNote, QualityStage, SetQuality};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::info;

use super::cache::{self, Cached};

#[derive(Clone, Copy, Default)]
pub(super) struct Options {
    pub replace: bool,
    pub stage_only: bool,
    pub allow_growth: bool,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostReport {
    key: String,
    roots: Vec<String>,
    fingerprint: String,
    extractor: String,
    fetched_at: u64,
    source_observed_at: Option<u64>,
    last_good_fetched_at: Option<u64>,
    last_good_fingerprint: Option<String>,
    found: usize,
    fetched: usize,
    skipped: usize,
    cap: usize,
    capped: bool,
    crawl_complete: bool,
    useful: usize,
    retained_records: usize,
    status: String,
}

pub(super) struct HostBatch {
    report: HostReport,
    hosts: BTreeSet<String>,
    pages: Vec<Article>,
    accepted: bool,
}

impl HostBatch {
    pub fn new(
        target: SitePagesTarget,
        cached: Cached,
        pages: Vec<Article>,
        min_useful: usize,
    ) -> Self {
        let e = cached.envelope;
        let hosts = target.roots.iter().filter_map(|r| host(r)).collect();
        let accepted = e.complete && pages.len() >= min_useful.max(1);
        let report = HostReport {
            key: e.key,
            roots: target.roots,
            fingerprint: e.fingerprint.clone(),
            extractor: e.extractor,
            last_good_fetched_at: accepted.then_some(e.fetched_at),
            last_good_fingerprint: accepted.then(|| e.fingerprint.clone()),
            fetched_at: e.fetched_at,
            source_observed_at: e.source_observed_at,
            found: e.found,
            fetched: e.docs.len(),
            skipped: e.skipped,
            cap: target.max_pages,
            capped: e.capped,
            crawl_complete: e.complete,
            useful: pages.len(),
            retained_records: 0,
            status: if !e.complete {
                "failed-fetch"
            } else if !accepted {
                "insufficient-useful-pages"
            } else if cached.reused {
                "reused"
            } else {
                "refreshed"
            }
            .into(),
        };
        Self {
            report,
            hosts,
            pages,
            accepted,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    schema: u32,
    set: String,
    generation: String,
    created_at: u64,
    /// Git revision supplied at build time, when available.
    source_revision: Option<String>,
    binary_version: String,
    base_sha256: Option<String>,
    extractor: String,
    sha256: String,
    records: usize,
    counts_by_host: BTreeMap<String, usize>,
    sources: Vec<HostReport>,
    failed_hosts: usize,
    layers: Vec<String>,
    quality: SetQuality,
}

fn host(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    Some(host.strip_prefix("www.").unwrap_or(&host).to_string())
}

fn article_host(article: &Article) -> Option<String> {
    article.item.as_deref().and_then(host)
}

fn read_pages(path: &Path) -> Result<Vec<Article>> {
    plumb_core::article::read_articles(plumb_ingest::open_maybe_gz(path)?, usize::MAX)
}

/// Keep the old host on failed, empty, or implausibly smaller refreshes.
/// A deliberate full replacement still requires each selected host to succeed.
fn merge(old: Vec<Article>, batches: &mut [HostBatch], replace: bool) -> Result<Vec<Article>> {
    for batch in batches.iter_mut() {
        let old_count = old
            .iter()
            .filter(|a| article_host(a).is_some_and(|h| batch.hosts.contains(&h)))
            .count();
        if !replace
            && !batch.report.capped
            && old_count > 0
            && batch.pages.len().saturating_mul(100) < old_count.saturating_mul(90)
        {
            batch.accepted = false;
            batch.report.status = "failed-shrink-guard".into();
            batch.report.last_good_fetched_at = None;
            batch.report.last_good_fingerprint = None;
        }
    }
    if replace && batches.iter().any(|b| !b.accepted) {
        bail!("full replacement has failed or insufficient hosts; previous set kept");
    }
    let replaced: BTreeSet<_> = batches
        .iter()
        .filter(|b| b.accepted && !b.report.capped)
        .flat_map(|b| b.hosts.iter().cloned())
        .collect();
    let updated_urls: BTreeSet<_> = batches
        .iter()
        .filter(|b| b.accepted)
        .flat_map(|b| b.pages.iter().filter_map(|p| p.item.clone()))
        .collect();
    for batch in batches
        .iter_mut()
        .filter(|b| b.accepted && b.report.capped && !replace)
    {
        batch.report.retained_records = old
            .iter()
            .filter(|p| {
                article_host(p).is_some_and(|h| batch.hosts.contains(&h))
                    && p.item
                        .as_ref()
                        .is_none_or(|url| !updated_urls.contains(url))
            })
            .count();
        if batch.report.retained_records > 0 {
            batch.report.last_good_fetched_at = None;
            batch.report.last_good_fingerprint = None;
        }
    }
    let mut pages = if replace {
        Vec::new()
    } else {
        old.into_iter()
            .filter(|a| article_host(a).is_none_or(|h| !replaced.contains(&h)))
            .filter(|a| {
                a.item
                    .as_ref()
                    .is_none_or(|url| !updated_urls.contains(url))
            })
            .collect()
    };
    for batch in batches.iter_mut().filter(|b| b.accepted) {
        pages.append(&mut batch.pages);
    }
    plumb_ingest::reference::sort_reference(&mut pages);
    Ok(pages)
}

fn suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0; 64 * 1024];
    loop {
        let len = file.read(&mut buf)?;
        if len == 0 {
            break;
        }
        hash.update(&buf[..len]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut part = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
    serde_json::to_writer_pretty(part.as_file_mut(), value)?;
    part.as_file_mut().write_all(b"\n")?;
    part.as_file().sync_all()?;
    part.persist(path)?;
    Ok(())
}

pub(super) fn publish(
    dest: &Path,
    set: &str,
    mut batches: Vec<HostBatch>,
    options: Options,
) -> Result<()> {
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    // Hold a cross-process advisory lock through merge, staging and promotion.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(suffix(dest, ".publish-lock"))?;
    lock.try_lock()
        .context("another publisher is refreshing this set")?;
    let base_sha256 = if dest.is_file() {
        Some(hash_file(dest)?)
    } else {
        None
    };
    let old = if dest.is_file() {
        read_pages(dest)?
    } else {
        Vec::new()
    };
    let old_layers = if dest.is_file() {
        crate::node::newer::layers_of(dest)?
    } else {
        Vec::new()
    };
    let previous: Option<Manifest> = std::fs::File::open(suffix(dest, ".manifest.json"))
        .ok()
        .and_then(|f| serde_json::from_reader(f).ok());
    // Bind retained records to their last good observation, not the failed
    // attempt's timestamp/configuration. Legacy observation dates stay unknown.
    let pages = merge(old, &mut batches, options.replace)?;
    if pages.is_empty() {
        bail!("no useful pages; previous set kept");
    }
    let generations = suffix(dest, ".generations");
    std::fs::create_dir_all(&generations)?;
    let stage = tempfile::Builder::new()
        .prefix("building-")
        .tempdir_in(&generations)?;
    let file = stage.path().join("pages.tsv.gz");
    plumb_ingest::articles::write_articles_file(&file, &pages)?;
    let validated = read_pages(&file)?;
    if validated.len() != pages.len()
        || validated
            .iter()
            .any(|a| a.title.trim().is_empty() || article_host(a).is_none())
    {
        bail!("generation failed article validation; previous set kept");
    }
    let layers = crate::node::newer::layers_of(&file)?;
    if !options.replace && old_layers.iter().any(|l| !layers.contains(l)) {
        bail!("generation would discard an existing enrichment layer; previous set kept");
    }
    if options.max_bytes > 0 && std::fs::metadata(&file)?.len() > options.max_bytes {
        bail!("candidate exceeds --max-set-bytes; previous set kept");
    }
    let sha256 = hash_file(&file)?;
    let mut counts_by_host = BTreeMap::new();
    for page in &pages {
        *counts_by_host
            .entry(article_host(page).context("validated URL")?)
            .or_default() += 1;
    }
    if let Some(previous) = &previous {
        for batch in batches
            .iter_mut()
            .filter(|b| !b.accepted || b.report.retained_records > 0)
        {
            if let Some(prior) = previous.sources.iter().find(|s| s.key == batch.report.key) {
                batch.report.last_good_fetched_at = prior.last_good_fetched_at;
                batch.report.last_good_fingerprint = prior.last_good_fingerprint.clone();
            }
        }
    }
    let selected: BTreeSet<_> = batches.iter().map(|b| b.report.key.clone()).collect();
    let mut sources = previous.map_or(Vec::new(), |m| {
        m.sources
            .into_iter()
            .filter(|r| !options.replace && !selected.contains(&r.key))
            .map(|mut r| {
                if ["refreshed", "reused", "preserved"].contains(&r.status.as_str()) {
                    r.status = "preserved".into();
                }
                r
            })
            .collect()
    });
    sources.extend(batches.into_iter().map(|b| b.report));
    sources.sort_by(|a, b| a.key.cmp(&b.key));
    // An unselected source's unresolved failure remains visible too.
    let failed_hosts = sources
        .iter()
        .filter(|s| !["refreshed", "reused", "preserved"].contains(&s.status.as_str()))
        .count();
    let known_hosts: BTreeSet<_> = sources
        .iter()
        .filter(|s| s.last_good_fetched_at.is_some())
        .flat_map(|s| s.roots.iter().filter_map(|r| host(r)))
        .collect();
    let provenance_complete = counts_by_host.keys().all(|h| known_hosts.contains(h));
    let created_at = cache::now();
    let generation = cache::digest(&serde_json::to_vec(&(&sha256, created_at, &sources))?);
    let capped = sources.iter().any(|s| s.capped);
    let fetched_at = if provenance_complete {
        sources
            .iter()
            .filter_map(|s| s.last_good_fetched_at)
            .min()
            .unwrap_or(0)
    } else {
        0
    };
    let quality = SetQuality {
        generation: generation.clone(),
        sha256: sha256.clone(),
        fetched_at,
        records: pages.len() as u64,
        hosts: counts_by_host.len() as u64,
        failed_hosts: failed_hosts as u64,
        capped,
        stages: vec![
            QualityStage {
                name: "source-refresh".into(),
                complete: failed_hosts == 0,
            },
            QualityStage {
                name: "useful-pages".into(),
                complete: failed_hosts == 0,
            },
            QualityStage {
                name: "article-validation".into(),
                complete: true,
            },
            QualityStage {
                name: "source-provenance".into(),
                complete: provenance_complete,
            },
        ],
    };
    let manifest = Manifest {
        schema: 1,
        set: set.into(),
        generation: generation.clone(),
        created_at,
        source_revision: option_env!("PLUMB_SOURCE_REVISION").map(str::to_string),
        binary_version: env!("CARGO_PKG_VERSION").into(),
        base_sha256,
        extractor: cache::EXTRACTOR_VERSION.into(),
        sha256,
        records: pages.len(),
        counts_by_host,
        sources,
        failed_hosts,
        layers,
        quality,
    };
    write_json(&stage.path().join("manifest.json"), &manifest)?;
    note_quality(&file, &manifest.quality)?;
    let generation_path = generations.join(&generation);
    if !generation_path.exists() {
        std::fs::rename(stage.path(), &generation_path)?;
        std::fs::File::open(&generations)?.sync_all()?;
    }
    info!(
        "staged {} records at {}",
        pages.len(),
        generation_path.display()
    );
    if !options.stage_only {
        promote_locked(&generation_path, dest, set, options)?;
    }
    Ok(())
}

pub(super) fn promote(generation: &Path, dest: &Path, set: &str, options: Options) -> Result<()> {
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(suffix(dest, ".publish-lock"))?;
    lock.try_lock()
        .context("another publisher is refreshing this set")?;
    promote_locked(generation, dest, set, options)
}

/// Whole-set builder hook: the owner supplies a semantic validator and
/// quality stages. This stages bytes; promotion remains a separate operation.
/// DOI items are valid here, unlike the URL-only inner-page host merge.
pub(super) fn stage_checked_articles(
    dest: &Path,
    set: &str,
    pages: &[Article],
    stages: Vec<QualityStage>,
    options: Options,
    validate: impl Fn(&[Article]) -> Result<()>,
) -> Result<PathBuf> {
    if stages.iter().any(|stage| !stage.complete) {
        bail!("builder validation/enrichment is incomplete; previous set kept");
    }
    validate(pages)?;
    if pages.is_empty() {
        bail!("empty candidate; previous set kept");
    }
    let generations = suffix(dest, ".generations");
    std::fs::create_dir_all(&generations)?;
    let stage = tempfile::Builder::new()
        .prefix("building-")
        .tempdir_in(&generations)?;
    let file = stage.path().join("pages.tsv.gz");
    plumb_ingest::articles::write_articles_file(&file, pages)?;
    let roundtrip = read_pages(&file)?;
    if roundtrip.len() != pages.len() {
        bail!("candidate lost records during serialization");
    }
    validate(&roundtrip)?;
    if options.max_bytes > 0 && std::fs::metadata(&file)?.len() > options.max_bytes {
        bail!("candidate exceeds --max-set-bytes; previous set kept");
    }
    let sha256 = hash_file(&file)?;
    let created_at = cache::now();
    let generation = cache::digest(&serde_json::to_vec(&(&sha256, created_at, &stages))?);
    let mut counts_by_host = BTreeMap::new();
    for host in pages.iter().filter_map(article_host) {
        *counts_by_host.entry(host).or_default() += 1;
    }
    let quality = SetQuality {
        generation: generation.clone(),
        sha256: sha256.clone(),
        fetched_at: created_at,
        records: pages.len() as u64,
        hosts: counts_by_host.len() as u64,
        failed_hosts: 0,
        capped: false,
        stages,
    };
    let manifest = Manifest {
        schema: 1,
        set: set.into(),
        generation: generation.clone(),
        created_at,
        source_revision: option_env!("PLUMB_SOURCE_REVISION").map(str::to_string),
        binary_version: env!("CARGO_PKG_VERSION").into(),
        base_sha256: if dest.is_file() {
            Some(hash_file(dest)?)
        } else {
            None
        },
        extractor: cache::EXTRACTOR_VERSION.into(),
        sha256,
        records: pages.len(),
        counts_by_host,
        sources: Vec::new(),
        failed_hosts: 0,
        layers: crate::node::newer::layers_of(&file)?,
        quality,
    };
    write_json(&stage.path().join("manifest.json"), &manifest)?;
    note_quality(&file, &manifest.quality)?;
    let path = generations.join(generation);
    if !path.exists() {
        std::fs::rename(stage.path(), &path)?;
    }
    std::fs::File::open(&generations)?.sync_all()?;
    Ok(path)
}

fn promote_locked(generation: &Path, dest: &Path, set: &str, options: Options) -> Result<()> {
    let manifest: Manifest =
        serde_json::from_reader(std::fs::File::open(generation.join("manifest.json"))?)?;
    let file = generation.join("pages.tsv.gz");
    if manifest.schema != 1 || manifest.set != set || hash_file(&file)? != manifest.sha256 {
        bail!("generation identity/checksum mismatch; previous set kept");
    }
    let current = if dest.is_file() {
        Some(hash_file(dest)?)
    } else {
        None
    };
    if !options.replace
        && current != manifest.base_sha256
        && current.as_deref() != Some(&manifest.sha256)
    {
        bail!("staged generation has a stale merge base; refresh again or use explicit --replace-set for rollback");
    }
    if options.max_bytes > 0 && std::fs::metadata(&file)?.len() > options.max_bytes {
        bail!("candidate exceeds --max-set-bytes; previous set kept");
    }
    let pages = read_pages(&file)?;
    if pages.len() != manifest.records {
        bail!("generation count mismatch");
    }
    if set == plumb_index::pages::PAPERS_SET {
        if manifest.quality.stages.iter().any(|stage| !stage.complete)
            || [
                "source-refresh",
                "canonical-paper-repair",
                "article-validation",
            ]
            .iter()
            .any(|name| {
                !manifest
                    .quality
                    .stages
                    .iter()
                    .any(|stage| &stage.name == name)
            })
        {
            bail!("paper generation lacks completed publication quality stages; previous set kept");
        }
        plumb_ingest::paper_validation::validate_landmarks(&pages)?;
    }
    drop(pages);
    if dest.is_file() {
        if set == plumb_index::pages::PAPERS_SET {
            let (modified, size) =
                crate::node::newer::stamp(dest).context("current paper stamp")?;
            if plumb_net::pages::read_quality(dest, modified, size).is_some_and(|quality| {
                quality
                    .stages
                    .iter()
                    .filter(|s| s.complete)
                    .any(|existing| {
                        !manifest
                            .quality
                            .stages
                            .iter()
                            .any(|candidate| candidate.name == existing.name && candidate.complete)
                    })
            }) {
                bail!("paper generation drops a completed quality stage; previous set kept");
            }
        }
        let layers = crate::node::newer::layers_of(dest)?;
        if layers.iter().any(|l| !manifest.layers.contains(l)) {
            bail!("generation lacks a current enrichment layer");
        }
        // Same 125% growth gate as automatic peer updates. Staging can exceed it
        // for evaluation, but publishing needs a consciously revised baseline.
        if !options.allow_growth
            && std::fs::metadata(&file)?.len().saturating_mul(100)
                > std::fs::metadata(dest)?.len().saturating_mul(125)
        {
            bail!("generation exceeds the 125% growth guard; staged for review (use --allow-set-growth after reviewing its resource cost)");
        }
    }
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // Copy through the existing atomic installer. Never move the advertised
    // pathname out of the way: interrupted promotion keeps a readable set.
    let part = tempfile::NamedTempFile::new_in(parent)?;
    std::fs::copy(&file, part.path())?;
    part.as_file().sync_all()?;
    let modified = crate::node::newer::stamp(&file)
        .context("staged file stamp")?
        .0;
    let previous_manifest = std::fs::read(suffix(dest, ".manifest.json")).ok();
    let previous_quality = std::fs::read(quality_path(dest)).ok();
    let transfer_notes = crate::pages::notes_path(dest);
    let previous_transfer_notes = std::fs::read(&transfer_notes).ok();
    let had_previous = dest.is_file();
    crate::node::newer::install(part.path(), dest, modified)?;
    let metadata = (|| -> Result<()> {
        let (modified, size) = crate::node::newer::stamp(dest).context("published file stamp")?;
        write_json(
            &quality_path(dest),
            &QualityNote {
                modified,
                size,
                quality: manifest.quality.clone(),
            },
        )?;
        // This local whole-set generation supersedes notes describing an
        // earlier peer transfer, including a truncated/incomplete transfer.
        if transfer_notes.exists() {
            std::fs::remove_file(&transfer_notes)?;
        }
        // The generation pointer is published last, after all complete bytes.
        write_json(&suffix(dest, ".manifest.json"), &manifest)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if let Err(error) = metadata {
        if had_previous {
            std::fs::rename(crate::node::newer::prev_path(dest), dest)?;
        } else {
            std::fs::remove_file(dest)?;
        }
        restore_note(&suffix(dest, ".manifest.json"), previous_manifest)?;
        restore_note(&quality_path(dest), previous_quality)?;
        restore_note(&transfer_notes, previous_transfer_notes)?;
        return Err(error.context("publication rolled back after metadata failure"));
    }
    info!(
        "published generation {} to {}",
        manifest.generation,
        dest.display()
    );
    Ok(())
}

fn note_quality(file: &Path, quality: &SetQuality) -> Result<()> {
    let (modified, size) = crate::node::newer::stamp(file).context("generation file stamp")?;
    write_json(
        &quality_path(file),
        &QualityNote {
            modified,
            size,
            quality: quality.clone(),
        },
    )
}

fn restore_note(path: &Path, bytes: Option<Vec<u8>>) -> Result<()> {
    if let Some(bytes) = bytes {
        std::fs::write(path, bytes)?;
    } else if path.is_file() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn page(host: &str, name: &str) -> Article {
        Article {
            title: name.into(),
            item: Some(format!("https://{host}/{name}")),
            ..Default::default()
        }
    }
    fn batch(host: &str, accepted: bool) -> HostBatch {
        HostBatch {
            report: HostReport {
                key: host.into(),
                roots: vec![],
                fingerprint: "test".into(),
                extractor: cache::EXTRACTOR_VERSION.into(),
                fetched_at: 100,
                source_observed_at: None,
                last_good_fetched_at: accepted.then_some(100),
                last_good_fingerprint: accepted.then(|| "test".into()),
                found: 1,
                fetched: 1,
                skipped: 0,
                cap: 1,
                capped: false,
                crawl_complete: accepted,
                useful: 1,
                retained_records: 0,
                status: if accepted { "refreshed" } else { "failed" }.into(),
            },
            hosts: [host.into()].into(),
            pages: vec![page(host, "new")],
            accepted,
        }
    }
    #[test]
    fn targeted_refresh_preserves_unselected_failed_and_shrunk_hosts() {
        let old = vec![
            page("python.org", "old"),
            page("rust.org", "keep"),
            page("failed.org", "good"),
        ];
        let mut batches = [batch("python.org", true), batch("failed.org", false)];
        let merged = merge(old, &mut batches, false).unwrap();
        let urls: BTreeSet<_> = merged.iter().map(|p| p.item.as_deref().unwrap()).collect();
        assert_eq!(
            urls,
            [
                "https://python.org/new",
                "https://rust.org/keep",
                "https://failed.org/good"
            ]
            .into()
        );
        let old = vec![page("python.org", "a"), page("python.org", "b")];
        let mut batches = [batch("python.org", true)];
        assert_eq!(merge(old.clone(), &mut batches, false).unwrap(), old);
        assert!(!batches[0].accepted);
        assert!(merge(vec![], &mut [batch("failed.org", false)], true).is_err());
        let mut capped = batch("python.org", true);
        capped.report.capped = true;
        let old = vec![page("python.org", "a"), page("python.org", "b")];
        let mut batches = [capped];
        let merged = merge(old, &mut batches, false).unwrap();
        assert_eq!(
            merged.len(),
            3,
            "capped refresh upserts without deleting unfetched URLs"
        );
        assert_eq!(batches[0].report.retained_records, 2);
        assert_eq!(batches[0].report.last_good_fetched_at, None);
    }
    #[test]
    fn staging_keeps_current_generation_and_promotion_validates_and_retains_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("docs.tsv.gz");
        publish(
            &dest,
            "docs",
            vec![batch("python.org", true)],
            Options::default(),
        )
        .unwrap();
        let original = std::fs::read(&dest).unwrap();
        let mut expansion = batch("rust.org", true);
        expansion.pages = (0..20)
            .map(|i| page("rust.org", &cache::digest(format!("task {i}").as_bytes())))
            .collect();
        publish(
            &dest,
            "docs",
            vec![expansion],
            Options {
                stage_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), original);
        let paths: Vec<_> = std::fs::read_dir(suffix(&dest, ".generations"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let next = paths
            .iter()
            .find(|p| read_pages(&p.join("pages.tsv.gz")).unwrap().len() == 21)
            .unwrap();
        // Larger candidate remains staged behind the growth guard.
        assert!(promote(next, &dest, "docs", Options::default()).is_err());
        let first = paths
            .iter()
            .find(|p| read_pages(&p.join("pages.tsv.gz")).unwrap().len() == 1)
            .unwrap();
        promote(first, &dest, "docs", Options::default()).unwrap();
        assert_eq!(
            std::fs::read(crate::node::newer::prev_path(&dest)).unwrap(),
            original
        );
        std::fs::write(first.join("pages.tsv.gz"), b"corrupt").unwrap();
        assert!(promote(first, &dest, "docs", Options::default()).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), original);
    }

    #[test]
    fn whole_set_hook_requires_semantic_validation_and_roundtrips_doi_items() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("papers.tsv.gz");
        let mut pages = vec![];
        let source: Vec<_> = plumb_ingest::paper_validation::LANDMARKS
            .iter()
            .map(|landmark| plumb_ingest::paper_names::ArxivPaper {
                id: landmark.id.into(),
                title: landmark.title.into(),
                year: landmark.submitted[..4].parse().ok(),
                authors: vec![landmark.first_author.into()],
                published: Some(landmark.submitted.into()),
                updated: None,
            })
            .collect();
        plumb_ingest::paper_validation::repair_landmarks(&mut pages, &source).unwrap();
        let stages = [
            "source-refresh",
            "canonical-paper-repair",
            "article-validation",
        ]
        .map(|name| QualityStage {
            name: name.into(),
            complete: true,
        })
        .to_vec();
        assert!(stage_checked_articles(
            &dest,
            "papers",
            &pages,
            stages.clone(),
            Options::default(),
            |_| bail!("bad canonical record")
        )
        .is_err());
        assert!(!dest.exists());
        let path =
            stage_checked_articles(&dest, "papers", &pages, stages, Options::default(), |p| {
                anyhow::ensure!(
                    p[0].title == pages[0].title && p[0].item == pages[0].item,
                    "canonical identity changed"
                );
                Ok(())
            })
            .unwrap();
        assert!(!dest.exists());
        promote(&path, &dest, "papers", Options::default()).unwrap();
        assert_eq!(read_pages(&dest).unwrap(), pages);
    }

    #[test]
    fn stale_targeted_generation_and_failed_metadata_do_not_replace_current_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let dest = crate::pages::SetInfo::find("docs")
            .unwrap()
            .file(dir.path());
        publish(
            &dest,
            "docs",
            vec![batch("python.org", true)],
            Options::default(),
        )
        .unwrap();
        let snapshot = crate::pages::Wanted::new(dir.path(), &crate::pages::PageSets::default(), 0);
        let snapshot_key = snapshot.key();
        let snapshot_file = snapshot.sets[0].1.clone();
        assert_ne!(
            snapshot_file, dest,
            "index builds read immutable generations"
        );
        publish(
            &dest,
            "docs",
            vec![batch("rust.org", true)],
            Options {
                stage_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        let staged = std::fs::read_dir(suffix(&dest, ".generations"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| read_pages(&p.join("pages.tsv.gz")).unwrap().len() == 2)
            .unwrap();
        let mut updated = batch("python.org", true);
        updated.pages = vec![page("python.org", "updated")];
        publish(&dest, "docs", vec![updated], Options::default()).unwrap();
        assert_eq!(
            snapshot.key(),
            snapshot_key,
            "an in-flight build keeps its generation"
        );
        assert_eq!(read_pages(&snapshot_file).unwrap()[0].title, "new");
        assert_ne!(
            crate::pages::Wanted::new(dir.path(), &crate::pages::PageSets::default(), 0).key(),
            snapshot_key
        );
        let current = std::fs::read(&dest).unwrap();
        let quality_before = std::fs::read(quality_path(&dest)).unwrap();
        let transfer_before = br#"{"lines":1,"complete":false,"source_modified":1,"fetched_at":1}"#;
        std::fs::write(crate::pages::notes_path(&dest), transfer_before).unwrap();
        assert!(promote(
            &staged,
            &dest,
            "docs",
            Options {
                allow_growth: true,
                ..Default::default()
            }
        )
        .is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), current);
        // Make the metadata publication fail after the data rename.
        let manifest_path = suffix(&dest, ".manifest.json");
        std::fs::remove_file(&manifest_path).unwrap();
        std::fs::create_dir(&manifest_path).unwrap();
        assert!(promote(
            &staged,
            &dest,
            "docs",
            Options {
                replace: true,
                allow_growth: true,
                ..Default::default()
            }
        )
        .is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), current);
        assert_eq!(std::fs::read(quality_path(&dest)).unwrap(), quality_before);
        assert_eq!(
            std::fs::read(crate::pages::notes_path(&dest)).unwrap(),
            transfer_before
        );
    }
}
