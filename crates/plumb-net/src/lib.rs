//! Plumb nodes talking to each other: sharing crawl results and answering
//! each other's searches. See `docs/network.md` for the design.
//!
//! * [`assign`]: which sites a node crawls each day.
//! * [`batch`]: signed crawl batches and what a node accepts from them.
//! * [`bucket`] and [`search`]: searching other nodes without sending the
//!   query.
//! * [`hash`]: hashes and the Merkle tree that proves one record of a batch.
//! * [`popularity`] and [`reports`]: sharing which site people pick for a
//!   search, readable only once many reports of the same pick are sent.
//! * [`store`]: the batches a node keeps.
//! * [`throwaway`]: one-request identities for searches and reports.
//! * [`proto`]: the messages and protocol names.
//! * [`node`]: the libp2p swarm and the [`NetHandle`] that drives it.

pub mod assign;
pub mod batch;
pub mod bucket;
pub mod hash;
pub mod node;
pub mod popularity;
pub mod proto;
pub mod reports;
pub mod search;
pub mod store;
pub mod throwaway;

pub use bucket::{BucketSource, BucketTable, BUCKETS_PER_SEARCH};
pub use libp2p::multiaddr::Protocol;
pub use libp2p::{Multiaddr, PeerId};
pub use node::{load_or_create_key, start, NetConfig, NetHandle, NetStatus};
pub use popularity::{PickLog, PopularityTable, Report};
pub use search::{FoundSite, NetSearch};
