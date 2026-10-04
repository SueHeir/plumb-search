//! Private information retrieval: fetching a bucket from a node without
//! the node learning which bucket it was. See `docs/reviews/pir-handoff.md`
//! for the direction and what PIR does not hide (the asker's address,
//! timing and how many requests it makes).
//!
//! Nothing here is used by searches yet: [`snapshot`] is the table a PIR
//! server will answer from.

pub mod snapshot;
