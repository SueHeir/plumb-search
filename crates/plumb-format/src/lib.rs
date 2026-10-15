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
//! how a manifest is signed and checked ([`sign`]), the key [`notes`] that
//! delegate, hand over and revoke signing keys, the [`deny`] list and how
//! it matches, and how the [`sites`] rules say which site a host is. It
//! only ever gains optional keys. It never names a source or a site, nor a
//! kind but `deny` and `site-rule`, the ones the core applies itself: what
//! an article or a package is belongs to the code that builds or ranks
//! it.
//!
//! plumb-node and plumb-index read the sets a node takes with it, and
//! plumb-core's built-in site rules are [`SiteRules`] too.

pub mod canonical;
pub mod deny;
pub mod item;
pub mod manifest;
pub mod notes;
pub mod refs;
pub mod sign;
pub mod sites;
pub mod urls;
pub mod value;

pub use deny::Denylist;
pub use item::{Deleted, Edge, Item, Vector};
pub use manifest::{FieldSpec, FilePart, Manifest};
pub use notes::Note;
pub use refs::Ref;
pub use sign::PublicKey;
pub use sites::SiteRules;
pub use value::FieldType;

/// The format every manifest names in its `format` key.
pub const FORMAT: &str = "plumb-set/1";
