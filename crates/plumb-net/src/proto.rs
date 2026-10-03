//! What nodes say to each other, and the names of the protocols they say it
//! over.
//!
//! * `/plumb/search/1`: a search ([`SearchRequest`]) and its best hits
//!   ([`SearchResponse`]), each hit with a [`RecordProof`] when the
//!   answering node holds a signed crawl of it.
//! * `/plumb/batch/1`: a batch by id, or the headers of the batches a node
//!   holds since an epoch, so a node that was away can catch up.
//! * Gossip topic `plumb/batches/1`: the [`SignedHeader`] of every new
//!   batch, as JSON. Nodes that want the batch fetch it with
//!   `/plumb/batch/1` from the node that passed the header on.
//! * `/plumb/kad/1.0.0`: Kademlia, to find more nodes.
//!
//! Requests and responses are CBOR.

use serde::{Deserialize, Serialize};

use crate::batch::{Batch, RecordProof, SignedHeader};
use crate::hash::Hash;

pub const SEARCH_PROTOCOL: &str = "/plumb/search/1";
pub const BATCH_PROTOCOL: &str = "/plumb/batch/1";
pub const KAD_PROTOCOL: &str = "/plumb/kad/1.0.0";
pub const IDENTIFY_PROTOCOL: &str = "/plumb/id/1.0.0";
pub const BATCH_TOPIC: &str = "plumb/batches/1";

/// Most hits a node returns for one search.
pub const MAX_SEARCH_HITS: u32 = 20;
/// Most batch headers returned for one [`BatchRequest::List`].
pub const MAX_LISTED_BATCHES: usize = 10_000;
/// Longest query accepted, in bytes.
pub const MAX_QUERY_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub hits: Vec<NetHit>,
}

/// A hit as one node sends it to another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetHit {
    pub domain: String,
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// The answering node's score. Only comparable between hits from the
    /// same node.
    pub score: f32,
    /// The signed crawl the hit's text comes from, when the node holds one.
    /// Without it the text comes from public seed data, or from a crawl the
    /// node cannot prove.
    pub proof: Option<RecordProof>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BatchRequest {
    /// The batch with this id.
    Get(Hash),
    /// The headers of the batches held from this epoch on.
    List { since_epoch: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BatchResponse {
    Batch(Option<Batch>),
    Headers(Vec<SignedHeader>),
}
