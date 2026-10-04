//! Plumb nodes talking to each other: sharing crawl results and answering
//! each other's searches. See `docs/network.md` for the design.
//!
//! * [`assign`]: which sites a node crawls each day.
//! * [`agree`]: records count only once two crawlers agree on them.
//! * [`credits`]: what crawling earns, and anonymous one-time tokens.
//! * [`batch`]: signed crawl batches and what a node accepts from them.
//! * [`bucket`] and [`search`]: searching other nodes without sending the
//!   query.
//! * [`oblivious`]: sending those requests sealed through a relay, so the
//!   node answering does not see who asks.
//! * [`rounds`]: bucket requests sent in the background at random times,
//!   built like a search's, so searches look like the rest of the traffic.
//! * [`joining`]: the default bootstrap nodes, and why a node is not
//!   connected.
//! * [`hash`]: hashes and the Merkle tree that proves one record of a batch.
//! * [`popularity`] and [`reports`]: sharing which site people pick for a
//!   search, readable only once many reports of the same pick are sent.
//! * [`store`]: the batches a node keeps.
//! * [`throwaway`]: one-request identities for searches and reports.
//! * [`proto`]: the messages and protocol names.
//! * [`node`]: the libp2p swarm and the [`NetHandle`] that drives it.

pub mod agree;
pub mod assign;
pub mod batch;
pub mod bucket;
pub mod cache;
pub mod credits;
pub mod hash;
pub mod joining;
pub mod node;
pub mod oblivious;
pub mod popularity;
pub mod proto;
pub mod reports;
pub mod rounds;
pub mod search;
pub mod store;
pub mod throwaway;

pub use bucket::{BucketSource, BucketTable, BUCKETS_PER_SEARCH};
pub use joining::{default_bootstrap, JoinProblem, PeerView, Route, DEFAULT_BOOTSTRAP};
pub use libp2p::multiaddr::Protocol;
pub use libp2p::{Multiaddr, PeerId};
pub use node::{load_or_create_key, start, CreditsAt, NetConfig, NetHandle, NetStatus};
pub use popularity::{PickLog, PopularityTable, Report};
pub use search::{FoundSite, NetSearch};
pub use store::CrawlerView;
