//! Embeddings: each site's text as a short list of numbers, so a search that
//! describes a site ("electric car maker") finds it by meaning, not only by
//! the words it shares with the site.
//!
//! Every node must get the same numbers for the same site, so that nodes can
//! check each other's by recomputing a few. Three things are pinned down:
//!
//! - **The text** ([`site_text`]): built the same way from the same record,
//!   byte for byte, and identified by its [`text_hash`].
//! - **The model** ([`Embedder`]): one small model, identified by the
//!   [`ModelId`] hash of its files; vectors of different models never mix.
//! - **The output**: each value is rounded to one byte ([`quantize`]), which
//!   absorbs the last-bit differences between CPUs. Vectors are compared by
//!   [`cosine`], so two honest nodes agree to well within 0.01.
//!
//! The model runs one text at a time, never in padded batches, so a site's
//! vector does not depend on which other sites were embedded with it.

mod model;
mod text;
mod vectors;

pub use model::{
    model_id, quantize, write_test_model, Embedder, ModelId, MAX_TOKENS, MODEL_BASE_URL,
    MODEL_FILES, MODEL_NAME,
};
pub use text::{site_text, text_hash, TextHash, MAX_TEXT_WORDS};
pub use vectors::{cosine, Vectors, VECTORS_FILE_NAME};
