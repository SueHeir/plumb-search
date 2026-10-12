//! The data format Plumb nodes, set builders and plugins exchange, version
//! `plumb-set/1`: items, the refs that join them, one canonical form for
//! URLs, and the signed manifest that describes a set.
//!
//! All of Plumb's shared data becomes one record, the [`Item`], plus one
//! signed [`Manifest`] per set, so a new source, kind or plugin needs data
//! rather than new code. An item is either a thing (an article, a crawled
//! homepage, a package) or an extension of another item, named by a
//! [`Ref`], that adds names, labels, fields, edges or vectors to it; a set
//! of extensions is a layer.
//!
//! This crate is the part of the format that is frozen: the record shapes,
//! the [`refs`] grammar, the [`urls`] canonical form (`url/1`) and match key,
//! the value formats in [`value`], the [`canonical`] JSON that is signed,
//! how a manifest is signed and checked ([`sign`]), and the key [`notes`]
//! that delegate, hand over and revoke signing keys. It only ever gains
//! optional keys. It never names a kind, a source or a site: what an
//! article or a package is belongs to the code that builds or ranks it.
//!
//! Nothing else in the workspace uses this crate yet. It is the first,
//! additive step of the v0.3.0 data format rework, so v0.2.0 nodes and
//! today's page sets are unaffected.

pub mod canonical;
pub mod item;
pub mod manifest;
pub mod notes;
pub mod refs;
pub mod sign;
pub mod urls;
pub mod value;

pub use item::{Deleted, Edge, Item, Vector};
pub use manifest::{FieldSpec, FilePart, Manifest};
pub use notes::Note;
pub use refs::Ref;
pub use sign::PublicKey;
pub use value::FieldType;

/// The format every manifest names in its `format` key.
pub const FORMAT: &str = "plumb-set/1";
