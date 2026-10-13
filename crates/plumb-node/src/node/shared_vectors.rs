//! Site vectors handed from node to node, so a node that turns search by
//! meaning on (or gets a new model) need not embed millions of sites on its
//! own CPUs: it takes the vectors another node made with the same model.
//!
//! They come as the project's signed set of that model's vectors
//! ([`vectors_set`]), from any node that holds it, over `/plumb/sets/1`
//! ([`super::signed_sets`]). The node reads it into its own vectors file
//! and removes it. While no node has the set, a trusted node's vectors
//! file comes over the page set protocol (`plumb_net::pages`), as
//! `vectors-<model id in hex>`: a node answers only when its own vectors
//! file was made by that model, and nodes that know nothing of this answer
//! that they have no such set. Either way a node keeps a vector only when
//! it was made from the very text the node has for that site (the text's
//! hash matches), so a vector is never kept for other text.

use std::path::{Path, PathBuf};
use std::sync::PoisonError;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use plumb_embed::{ModelId, TextHash, Vectors};
use plumb_format::Ref;
use plumb_net::pages::MAX_PAGES_CHUNK;
use plumb_net::NetHandle;
use tracing::{debug, info, warn};

use super::control::{hex_decode, hex_encode};
use super::signed_sets::Looked;
use super::Inner;
use crate::meaning::{vectors_set, wanted_texts, MeaningIndex};

/// The start of the set name vectors go by.
const SET_PREFIX: &str = "vectors-";
/// Where a download goes until it is read.
pub(super) const PART_FILE: &str = "vectors-taken.bin.part";
/// Wait after a node said it was busy.
const BUSY_WAIT: Duration = Duration::from_secs(5);
/// Vectors taken that are put in at once.
const PUT_IN_AT_ONCE: usize = 10_000;

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

/// Takes the vectors another node made with this node's model, and keeps
/// those made from the text this node has for sites lacking an up-to-date
/// vector.
pub(super) fn take(
    inner: &Inner,
    net: &NetHandle,
    meaning: &MeaningIndex,
    vectors_path: &Path,
) -> Result<Taken> {
    if let Some(pause) = inner.download_pause() {
        debug!("site vectors not downloaded now: {}", pause.reason);
        return Ok(Taken::Kept(0));
    }
    if let Some(kept) = take_set(inner, net, meaning, vectors_path)? {
        return Ok(Taken::Kept(kept));
    }
    let model = meaning.embedder().id();
    let dim = meaning.embedder().dim();
    let part = inner.paths.data.join(PART_FILE);
    let downloaded = download(inner, net, &set_name(&model), &part);
    let result = downloaded.and_then(|got| match got {
        Download::NoNode => Ok(Taken::NoNode),
        Download::NoFile => Ok(Taken::Kept(0)),
        Download::From(peer) => {
            let (their_model, their_dim, _) = Vectors::read_header(&part)?;
            if their_model != model || their_dim != dim {
                bail!("{peer} sent vectors of another model");
            }
            let read = |each: &mut Each| Vectors::for_each_in(&part, each).map(drop);
            keep(inner, meaning, vectors_path, &peer, read).map(Taken::Kept)
        }
    });
    let _ = plumb_core::storage::remove_file(&part, inner.storage.as_deref());
    result
}

/// Takes the signed set of the vectors this node's model makes, when a
/// node has one a trusted publisher signed that is newer than any this
/// node holds, and keeps the vectors this node wants from it; `None` when
/// it took none. A set taken is removed once read: the vectors kept are in
/// the node's own file. One the node holds already, as its publisher's
/// own node does to hand it on, is left as it is.
fn take_set(
    inner: &Inner,
    net: &NetHandle,
    meaning: &MeaningIndex,
    vectors_path: &Path,
) -> Result<Option<usize>> {
    let set = vectors_set(&meaning.embedder().id());
    let runtime = tokio::runtime::Handle::current();
    match runtime.block_on(super::signed_sets::look(inner, net, &set)) {
        Ok(Looked::Took(_)) => {}
        Ok(_) => return Ok(None),
        Err(err) => {
            warn!("search by meaning: taking {set}: {err:#}");
            return Ok(None);
        }
    }
    let dir = super::signed_sets::set_dir(&inner.paths.data, &set).context("not a set name")?;
    let (model, dim) = (
        hex_encode(&meaning.embedder().id()),
        meaning.embedder().dim(),
    );
    let read = |each: &mut Each| {
        crate::sets::for_each_item(&dir, |item| {
            let Some(Ref::Site(domain)) = &item.extends else {
                return Ok(());
            };
            for vector in item.vectors.iter().filter(|vector| vector.model == model) {
                let of = hex_decode(&vector.of)
                    .ok()
                    .and_then(|of| of.try_into().ok());
                let values = vector.values().filter(|values| values.len() == dim);
                if let (Some(of), Some(values)) = (of, values) {
                    each(domain, &of, &values)?;
                }
            }
            Ok(())
        })
        .map(drop)
    };
    let kept = keep(inner, meaning, vectors_path, &set, read);
    let _ = plumb_core::storage::remove_directory(&dir, inner.storage.as_deref());
    kept.map(Some)
}

/// What hands [`keep`] each vector read: its site, the hash of the text
/// it was made from, and its values.
type Each<'a> = dyn FnMut(&str, &TextHash, &[i8]) -> Result<()> + 'a;

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
        plumb_core::storage::BudgetFile::create(part, inner.storage.clone())
            .with_context(|| format!("creating {}", part.display()))?,
    );
    let (size, modified) = (first.size, first.modified);
    let mut offset = 0u64;
    let mut chunk = first;
    loop {
        out.write_all(&chunk.bytes)?;
        offset += chunk.bytes.len() as u64;
        if let Err(err) = inner.add_downloaded(chunk.bytes.len() as u64) {
            warn!("site vectors: counting the download: {err:#}");
        }
        if offset >= size || chunk.bytes.is_empty() {
            break;
        }
        if inner.stopping() || inner.owner_pause().is_some() {
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
    out.into_inner()
        .map_err(|err| err.into_error())?
        .sync_all()?;
    Ok(Download::From(from.to_string()))
}

/// Keeps the vectors `read` hands over, from `from`, that this node wants.
fn keep(
    inner: &Inner,
    meaning: &MeaningIndex,
    vectors_path: &Path,
    from: &str,
    read: impl FnOnce(&mut Each) -> Result<()>,
) -> Result<usize> {
    let wanted = {
        let _records = inner.hold_records();
        wanted_texts(
            meaning.vectors(),
            &inner.paths.records,
            meaning.embedder().text_words(),
            inner.storage.clone(),
        )?
    };
    // Read a batch, then put it in: search by meaning waits on the lock,
    // not on reading the file, and only a batch is held twice.
    let mut taken = Vec::new();
    let mut kept = 0;
    let mut put_in = |taken: &mut Vec<(String, plumb_embed::TextHash, Vec<i8>)>| {
        let mut vectors = meaning
            .vectors()
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        for (domain, hash, vector) in taken.drain(..) {
            vectors.insert(&domain, hash, &vector)?;
            kept += 1;
        }
        anyhow::Ok(())
    };
    read(&mut |domain, hash, vector| {
        if wanted.get(domain) == Some(hash) {
            taken.push((domain.to_string(), *hash, vector.to_vec()));
            if taken.len() >= PUT_IN_AT_ONCE {
                put_in(&mut taken)?;
            }
        }
        Ok(())
    })?;
    put_in(&mut taken)?;
    if kept > 0 {
        Vectors::save_shared_with_budget(meaning.vectors(), vectors_path, inner.storage.clone())?;
    }
    info!(
        "search by meaning: kept {kept} site vectors from {from} ({} wanted)",
        wanted.len()
    );
    inner.journal.info(format!(
        "Search by meaning: {} site vectors taken from another node",
        crate::web::group_thousands(kept as u64)
    ));
    Ok(kept)
}
