//! What nodes say to each other, and the names of the protocols they say it
//! over.
//!
//! * `/plumb/bucket/1`: one bucket of sites ([`BucketRequest`]); the query
//!   itself never leaves the asking node (see [`crate::bucket`]). Each site
//!   comes with a [`RecordProof`] when the answering node holds a signed
//!   crawl of it, and with proofs from other crawlers that agree with it
//!   when it holds those. Asked under a throwaway identity, over a connection of
//!   its own.
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

pub const BUCKET_PROTOCOL: &str = "/plumb/bucket/1";
pub const BATCH_PROTOCOL: &str = "/plumb/batch/1";
pub const KAD_PROTOCOL: &str = "/plumb/kad/1.0.0";
pub const IDENTIFY_PROTOCOL: &str = "/plumb/id/1.0.0";
pub const BATCH_TOPIC: &str = "plumb/batches/1";

/// Most batch headers returned for one [`BatchRequest::List`].
pub const MAX_LISTED_BATCHES: usize = 10_000;

/// Asks for one bucket (see [`crate::bucket`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketRequest {
    pub bucket: u32,
}

/// A bucket's records; `None` from a node that has no bucket table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketResponse {
    pub records: Option<Vec<BucketRecord>>,
}

/// One site of a bucket, with the proof of its signed crawl when the
/// answering node holds one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketRecord {
    /// The site's record, as JSON.
    pub record: String,
    pub proof: Option<RecordProof>,
    /// Proofs of other crawlers' crawls of the same site that agree with
    /// `proof` (see [`crate::agree`]), at most [`MAX_EXTRA_PROOFS`]. Nodes
    /// that predate them send none and ignore them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also: Vec<RecordProof>,
}

/// Most proofs from other crawlers sent with one site of a bucket.
pub const MAX_EXTRA_PROOFS: usize = crate::agree::QUORUM - 1;

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
