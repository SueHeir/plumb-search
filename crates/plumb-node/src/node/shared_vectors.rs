//! Site vectors handed from node to node, so a node that turns search by
//! meaning on (or gets a new model) need not embed millions of sites on its
//! own CPUs: it takes the vectors a trusted node made with the same model.
//!
//! The vectors file goes over the page set protocol (`plumb_net::pages`),
//! as the set `vectors-<model id in hex>`: a node answers only when its own
//! vectors file was made by that model, and nodes that know nothing of
//! this answer that they have no such set. Like page sets, vectors are
//! taken only from trusted nodes. A node keeps a vector it is given only
//! when it was made from the very text the node has for that site (the
//! text's hash matches), so a vector is never kept for other text.

use std::path::{Path, PathBuf};
use std::sync::PoisonError;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use plumb_embed::{ModelId, Vectors};
use plumb_net::pages::MAX_PAGES_CHUNK;
use plumb_net::NetHandle;
use tracing::{debug, info};

use super::control::hex_encode;
use super::Inner;
use crate::meaning::{wanted_texts, MeaningIndex};

/// The start of the set name vectors go by.
const SET_PREFIX: &str = "vectors-";
/// Where a download goes until it is read.
pub(super) const PART_FILE: &str = "vectors-taken.bin.part";
/// Wait after a node said it was busy.
const BUSY_WAIT: Duration = Duration::from_secs(5);

/// The set name of the vectors made by `model`.
pub(super) fn set_name(model: &ModelId) -> String {
    format!("{SET_PREFIX}{}", hex_encode(model))
}

/// The vectors file to hand a node asking for `set`, when `set` names
/// vectors and this node's were made by that model.
pub(super) fn servable(data: &Path, set: &str) -> Option<PathBuf> {
    set.strip_prefix(SET_PREFIX)?;
    let path = data.join(plumb_embed::VECTORS_FILE_NAME);
    let (model, _, count) = Vectors::read_header(&path).ok()?;
    (count > 0 && set_name(&model) == set).then_some(path)
}

/// What [`take`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Taken {
    /// No trusted node that hands on files is connected.
    NoNode,
    /// The vectors kept; 0 when no trusted node had any of this model.
    Kept(usize),
}

/// Takes the vectors a trusted node made with this node's model, and keeps
/// those made from the text this node has for sites lacking an up-to-date
/// vector.
pub(super) fn take(
    inner: &Inner,
    net: &NetHandle,
    meaning: &MeaningIndex,
    vectors_path: &Path,
) -> Result<Taken> {
    let model = meaning.embedder().id();
    let dim = meaning.embedder().dim();
    let set = set_name(&model);
    let part = inner.paths.data.join(PART_FILE);
    let downloaded = download(inner, net, &set, &part);
    let result = downloaded.and_then(|got| match got {
        Download::NoNode => Ok(Taken::NoNode),
        Download::NoFile => Ok(Taken::Kept(0)),
        Download::From(peer) => {
            keep(inner, meaning, &part, model, dim, vectors_path, &peer).map(Taken::Kept)
        }
    });
    let _ = std::fs::remove_file(&part);
    result
}

enum Download {
    NoNode,
    /// No trusted node has the file, or this node stopped.
    NoFile,
    /// Downloaded from that node.
    From(String),
}

/// Downloads the vectors file `set` from a trusted node into `part`.
fn download(inner: &Inner, net: &NetHandle, set: &str, part: &Path) -> Result<Download> {
    use std::io::Write;

    let runtime = tokio::runtime::Handle::current();
    let mut lacking = Vec::new();
    let first = loop {
        match runtime.block_on(net.pages_chunk(set, 0, MAX_PAGES_CHUNK, None, &lacking))? {
            None if lacking.is_empty() => return Ok(Download::NoNode),
            None => {
                debug!(
                    "no trusted node has vectors of this model ({} asked)",
                    lacking.len()
                );
                return Ok(Download::NoFile);
            }
            Some(chunk) if chunk.busy => {
                if !super::embedding::nap_until_stop(inner, BUSY_WAIT) {
                    return Ok(Download::NoFile);
                }
            }
            Some(chunk) if chunk.size == 0 => lacking.push(chunk.peer),
            Some(chunk) => break chunk,
        }
    };
    let from = first.peer;
    info!(
        "taking site vectors from {from}: {} MB",
        first.size.div_ceil(1_000_000)
    );
    inner.journal.info(format!(
        "Search by meaning: downloading site vectors from a node you trust ({} MB)",
        first.size.div_ceil(1_000_000)
    ));
    let mut out = std::io::BufWriter::new(
        std::fs::File::create(part).with_context(|| format!("creating {}", part.display()))?,
    );
    let (size, modified) = (first.size, first.modified);
    let mut offset = 0u64;
    let mut chunk = first;
    loop {
        out.write_all(&chunk.bytes)?;
        offset += chunk.bytes.len() as u64;
        if offset >= size || chunk.bytes.is_empty() {
            break;
        }
        if inner.stopping() {
            return Ok(Download::NoFile);
        }
        chunk = loop {
            match runtime.block_on(net.pages_chunk(
                set,
                offset,
                MAX_PAGES_CHUNK,
                Some(from),
                &[],
            ))? {
                None => bail!("{from} went away while its vectors were taken"),
                Some(next) if next.busy => {
                    if !super::embedding::nap_until_stop(inner, BUSY_WAIT) {
                        return Ok(Download::NoFile);
                    }
                }
                Some(next) if next.size != size || next.modified != modified => {
                    bail!("{from} saved new vectors while they were taken")
                }
                Some(next) => break next,
            }
        };
    }
    if offset != size {
        bail!("{from} sent {offset} of {size} bytes of vectors");
    }
    out.into_inner()?.sync_all()?;
    Ok(Download::From(from.to_string()))
}

/// Keeps the vectors in the downloaded file `part` that this node wants.
fn keep(
    inner: &Inner,
    meaning: &MeaningIndex,
    part: &Path,
    model: ModelId,
    dim: usize,
    vectors_path: &Path,
    from: &str,
) -> Result<usize> {
    let (their_model, their_dim, _) = Vectors::read_header(part)?;
    if their_model != model || their_dim != dim {
        bail!("{from} sent vectors of another model");
    }
    let wanted = {
        let _records = inner.hold_records();
        wanted_texts(
            meaning.vectors(),
            &inner.paths.records,
            meaning.embedder().text_words(),
        )?
    };
    let mut kept = 0usize;
    {
        let mut vectors = meaning
            .vectors()
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        Vectors::for_each_in(part, |domain, hash, vector| {
            if wanted.get(domain) == Some(hash) {
                vectors.insert(domain, *hash, vector)?;
                kept += 1;
            }
            Ok(())
        })?;
    }
    if kept > 0 {
        meaning
            .vectors()
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .save(vectors_path)?;
    }
    info!(
        "search by meaning: kept {kept} site vectors from {from} ({} wanted)",
        wanted.len()
    );
    inner.journal.info(format!(
        "Search by meaning: {} site vectors taken from a node you trust",
        crate::web::group_thousands(kept as u64)
    ));
    Ok(kept)
}
