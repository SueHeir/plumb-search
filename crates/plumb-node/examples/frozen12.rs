//! Fixed-cohort observations through the production router, without a listener.
//! See docs/frozen12-adapter.md for the separate execution/retention contract.

#[path = "frozen12/backend.rs"]
mod backend;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use axum::body::{to_bytes, Body};
use axum::http::Request;
use clap::Parser;
use plumb_index::retained::{FileBinding, GenerationBinding, RetainedGeneration};
use plumb_index::{RankConfig, SearchOptions, Searcher};
use plumb_node::cli::{MeaningArgs, QueryInstruction};
use plumb_node::meaning::{MeaningIndex, SharedMeaning};
use plumb_node::web::{self, IndexBackend};
use plumb_node::websearch::WebSettings;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const MAX_REPORT: usize = 16 << 20;
const MAX_RESPONSE: usize = 1 << 20;
const OPTIONS: &str = "full=1&limit=10&lang=en&country=US&only=0&exact=0&safe=off&news=off&net=0";
const QUERIES: [&str; 12] = [
    "Find the official website of the Rust programming language.",
    "Find the official US Bank personal banking website.",
    "Find the official documentation for the Python package installer pip.",
    "All the Mods 10 on Minecraft 1.21.1: what are the early-game flight options and prerequisites?",
    "Questie in WoW Classic Era: how do I enable quest tracking?",
    "Find the DBM package that supports raid warnings in WoW Classic Era.",
    "What is Mount Everest’s agreed height above sea level in metres?",
    "Who wrote the original novel Pride and Prejudice?",
    "What is Japan’s national capital city?",
    "Find restaurants within 2 km of Denver Union Station, with names and map locations.",
    "Find public libraries in Denver, Colorado, with street addresses and map locations.",
    "Find museums in Denver, Colorado, with names and map locations.",
];

#[derive(Parser)]
struct Args {
    /// Reviewed binding JSON, at most 1 MiB. Does not grant execution permission.
    #[arg(long)]
    manifest: PathBuf,
    /// New task-owned report; existing files are never overwritten.
    #[arg(long)]
    report: PathBuf,
    /// Optional completed baseline observations; corpus/options must match exactly.
    #[arg(long)]
    baseline_report: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexInput {
    path: PathBuf,
    binding: GenerationBinding,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MeaningInput {
    model: PathBuf,
    model_files: BTreeMap<String, FileBinding>,
    vectors: PathBuf,
    vectors_binding: FileBinding,
    retention_receipt: String,
    query_instruction: String,
}

impl MeaningInput {
    fn verify(&self) -> Result<()> {
        ensure!(
            !self.retention_receipt.is_empty(),
            "model/vector retention required"
        );
        ensure!(
            !self.model.join(plumb_embed::SERVER_FILE).exists(),
            "embedding servers excluded"
        );
        let names: Vec<&str> = if self.model.join(plumb_embed::GEMMA_FILE).exists() {
            vec![plumb_embed::GEMMA_FILE, plumb_embed::GEMMA_TOKENIZER_FILE]
        } else {
            plumb_embed::MODEL_FILES.to_vec()
        };
        ensure!(
            self.model_files.len() == names.len(),
            "exact model catalog required"
        );
        for name in names {
            let file = self
                .model_files
                .get(name)
                .context("model file binding missing")?;
            ensure!(
                valid_sha(file.sha256.as_deref()),
                "externally attested model SHA required"
            );
            file.verify_identity(&self.model.join(name))?;
        }
        ensure!(
            valid_sha(self.vectors_binding.sha256.as_deref()),
            "externally attested vectors SHA required"
        );
        self.vectors_binding.verify_identity(&self.vectors)?;
        Ok(())
    }
    fn args(&self) -> Result<MeaningArgs> {
        let query_instruction = match self.query_instruction.as_str() {
            "off" => QueryInstruction::Off,
            "on" => QueryInstruction::On,
            "mix" => QueryInstruction::Mix,
            "min" => QueryInstruction::Min,
            "split" => QueryInstruction::Split,
            _ => anyhow::bail!("unknown query instruction"),
        };
        Ok(MeaningArgs {
            model: Some(self.model.clone()),
            vectors: Some(self.vectors.clone()),
            query_instruction,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    /// Separately issued run approval/resource lease, recorded as an attestation.
    execution_receipt: String,
    revision: String,
    /// Externally verified Git tree: the binary embeds revision/source SHA only.
    tree: String,
    source_sha256: String,
    binary_sha256: String,
    sites: IndexInput,
    pages: IndexInput,
    places: Option<IndexInput>,
    meaning: Option<MeaningInput>,
    rank: RankConfig,
}

fn valid_sha(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn bounded_json(path: &Path, limit: usize) -> Result<Value> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "JSON exceeds byte limit");
    Ok(serde_json::from_slice(&bytes)?)
}

/// Retrieval/corpus bindings deliberately exclude candidate build identity.
fn comparison_binding(manifest: &Manifest) -> Value {
    json!({ "version": 1, "cohort": "frozen12-known-regression", "queries": QUERIES,
        "options": OPTIONS, "sites": manifest.sites, "pages": manifest.pages,
        "places": manifest.places, "meaning": manifest.meaning, "rank": manifest.rank,
        "network_popularity": "disabled", "adult_list": "disabled-safe-off",
        "plugins": "none", "findings": "none", "news": "off",
        "learned_model_sha256": format!("{:x}", Sha256::digest(include_bytes!("../../plumb-index/src/learned_model.json"))),
        "clock": "system-observed-no-time-queries" })
}

fn validate_baseline(baseline: &Value, binding: &Value) -> Result<()> {
    ensure!(
        baseline["status"] == "complete" && baseline["comparison_binding"] == *binding,
        "baseline incomplete or corpus/options bindings differ"
    );
    ensure!(
        baseline["observations"]
            .as_array()
            .is_some_and(|v| v.len() == 12),
        "baseline cohort incomplete"
    );
    Ok(())
}

fn request_uri(query: &str) -> String {
    let q = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", query)
        .finish();
    format!("/api/search?{q}&{OPTIONS}")
}

fn frozen_options() -> SearchOptions {
    SearchOptions {
        country: Some("US".into()),
        language: Some("en".into()),
        safe: plumb_core::SafeSearch::Off,
        recent: plumb_core::RecentNews::Off,
        ..Default::default()
    }
}

async fn observe(app: axum::Router, query: &str) -> Result<Value> {
    ensure!(
        !plumb_answer::may_need_rates(query)
            && plumb_answer::weather::asked(query).is_none()
            && plumb_node::websearch::bang_url(query).is_none(),
        "external query excluded"
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri(request_uri(query))
                .body(Body::empty())?,
        )
        .await?;
    ensure!(
        response.status().is_success(),
        "router status {}",
        response.status()
    );
    let bytes = to_bytes(response.into_body(), MAX_RESPONSE).await?;
    let body: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        body["assembled"]["rows"].is_array(),
        "missing production assembled rows"
    );
    Ok(body)
}

fn finish_report(report: &mut Value, errors: &[String]) {
    report["errors"] = json!(errors);
    report["status"] = json!(if errors.is_empty() {
        "complete"
    } else {
        "incomplete"
    });
    if !errors.is_empty() {
        report["observations"] = json!([]);
    }
}

fn binary_hash() -> Result<String> {
    let mut file = std::fs::File::open(std::env::current_exe()?)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn run(args: Args) -> Result<()> {
    let started = Instant::now();
    let manifest: Manifest = serde_json::from_value(bounded_json(&args.manifest, 1 << 20)?)?;
    ensure!(
        manifest.version == 1 && !manifest.execution_receipt.is_empty(),
        "reviewed v1 execution receipt required"
    );
    let build = plumb_node::build_info::current();
    ensure!(
        build.dirty == Some(false)
            && build.revision == manifest.revision
            && build.source_sha256.as_deref() == Some(manifest.source_sha256.as_str()),
        "exact clean build binding required"
    );
    ensure!(
        manifest.tree.len() == 40 && manifest.tree.bytes().all(|b| b.is_ascii_hexdigit()),
        "declared tree required"
    );
    let binary_sha256 = binary_hash()?;
    ensure!(
        valid_sha(Some(&manifest.binary_sha256)) && binary_sha256 == manifest.binary_sha256,
        "binary binding differs"
    );
    let binding = comparison_binding(&manifest);
    if let Some(baseline) = &args.baseline_report {
        validate_baseline(&bounded_json(baseline, MAX_REPORT)?, &binding)?;
    }
    // No output may be inside any corpus/model directory; no parent creation.
    let output_parent = args
        .report
        .parent()
        .context("report parent required")?
        .canonicalize()?;
    let mut inputs = vec![&manifest.sites.path, &manifest.pages.path];
    if let Some(places) = &manifest.places {
        inputs.push(&places.path);
    }
    if let Some(meaning) = &manifest.meaning {
        inputs.push(&meaning.model);
    }
    for path in inputs {
        ensure!(
            !output_parent.starts_with(path.canonicalize()?),
            "report is inside retained input"
        );
    }
    if let Some(meaning) = &manifest.meaning {
        ensure!(args.report != meaning.vectors, "report is vectors input");
    }
    let mut report_options = std::fs::OpenOptions::new();
    report_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        report_options.mode(0o600);
    }
    let mut report_file = report_options.open(&args.report)?;
    let mut report = json!({ "version": 1, "status": "incomplete", "build": build,
        "declared_tree": manifest.tree, "binary_sha256": binary_sha256,
        "execution_receipt_attested": manifest.execution_receipt,
        "comparison_binding": binding, "started_unix": plumb_core::now_unix(),
        "judgments": "unknown; observations only; no grade inheritance",
        "integrity": "metadata SHA verified; segments/model/vectors identity checked; large-file SHA externally attested",
        "retention": "external immutable generation guarantee required; in-memory META_LOCK does not exclude live writers",
        "resource_enforcement": "cooperative wall checks only; separately leased external watchdog required",
        "observations": [], "errors": [] });
    let mut observations = Vec::new();
    let mut errors = Vec::new();
    let attempt = (|| -> Result<()> {
        let sites = RetainedGeneration::open(&manifest.sites.path, manifest.sites.binding.clone())?;
        let pages = RetainedGeneration::open(&manifest.pages.path, manifest.pages.binding.clone())?;
        let places = manifest
            .places
            .as_ref()
            .map(|p| RetainedGeneration::open(&p.path, p.binding.clone()))
            .transpose()?;
        if let Some(meaning) = &manifest.meaning {
            meaning.verify()?;
        }
        let searcher = Searcher::open_retained(&sites)?;
        ensure!(
            sites
                .binding()
                .files
                .contains_key(plumb_index::spell_model::MODEL_FILE)
                == searcher.spelling_model().is_some(),
            "spelling model missing or unreadable"
        );
        let meaning = manifest
            .meaning
            .as_ref()
            .map(|m| MeaningIndex::from_args(&m.args()?))
            .transpose()?
            .flatten();
        let index =
            IndexBackend::new(searcher, manifest.rank).with_meaning(SharedMeaning::new(meaning));
        let place_searcher = places
            .as_ref()
            .map(plumb_index::places::PlaceSearcher::open_retained)
            .transpose()?;
        let backend = Arc::new(backend::FrozenBackend {
            sites: index,
            pages: plumb_index::pages::PageSearcher::open_retained(&pages)?,
            rank: manifest.rank,
            places: place_searcher,
            raw: Default::default(),
            primary: Default::default(),
            errors: Default::default(),
            #[cfg(test)]
            injected_auxiliary_error: Default::default(),
        });
        let app = web::router_with(
            backend.clone(),
            WebSettings {
                home: plumb_node::country::HomeCountry::Fixed("US".into()),
                language: Some("en".into()),
                ..Default::default()
            },
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            for (i, query) in QUERIES.iter().enumerate() {
                ensure!(
                    started.elapsed() <= Duration::from_secs(600),
                    "run wall limit"
                );
                sites.verify()?;
                pages.verify()?;
                if let Some(places) = &places {
                    places.verify()?;
                }
                if let Some(meaning) = &manifest.meaning {
                    meaning.verify()?;
                }
                backend.begin_primary(query, 10, frozen_options());
                let before = Instant::now();
                let body =
                    tokio::time::timeout(Duration::from_secs(30), observe(app.clone(), query))
                        .await??;
                ensure!(
                    before.elapsed() <= Duration::from_secs(30),
                    "query wall limit"
                );
                let lookup_errors = std::mem::take(&mut *backend.errors.lock().unwrap());
                ensure!(
                    lookup_errors.is_empty(),
                    "lookup errors: {}",
                    lookup_errors.join("; ")
                );
                let raw = backend
                    .raw
                    .lock()
                    .unwrap()
                    .take()
                    .context("missing raw retrieval observation")?;
                observations.push(json!({ "id": format!("PG{:02}", i + 1), "query": query,
                    "request": request_uri(query), "elapsed_ms": before.elapsed().as_millis(),
                    "raw_request": backend.primary.lock().unwrap().as_ref(),
                    "raw_retrieval": raw, "body": body }));
            }
            Ok::<(), anyhow::Error>(())
        })?;
        sites.verify()?;
        pages.verify()?;
        if let Some(places) = &places {
            places.verify()?;
        }
        if let Some(meaning) = &manifest.meaning {
            meaning.verify()?;
        }
        ensure!(
            started.elapsed() <= Duration::from_secs(600),
            "final run wall limit"
        );
        Ok(())
    })();
    if let Err(error) = attempt {
        errors.push(format!("{error:#}"));
    }
    report["observations"] = json!(observations);
    report["finished_unix"] = json!(plumb_core::now_unix());
    report["elapsed_ms"] = json!(started.elapsed().as_millis());
    finish_report(&mut report, &errors);
    let mut bytes = serde_json::to_vec_pretty(&report)?;
    if bytes.len() > MAX_REPORT || started.elapsed() > Duration::from_secs(600) {
        errors.push("final serialization exceeded report or run wall limit".into());
        finish_report(&mut report, &errors);
        bytes = serde_json::to_vec_pretty(&report)?;
    }
    ensure!(
        bytes.len() <= MAX_REPORT,
        "binding metadata exceeds report cap"
    );
    report_file.write_all(&bytes)?;
    report_file.sync_all()?;
    ensure!(
        errors.is_empty(),
        "frozen12 incomplete; see {}",
        args.report.display()
    );
    Ok(())
}

fn main() -> Result<()> {
    run(Args::parse())
}

#[cfg(test)]
#[path = "frozen12/tests.rs"]
mod tests;
