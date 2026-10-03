//! What nodes say to each other, and the names of the protocols they say it
//! over.
//!
//! * `/plumb/bucket/1`: one bucket of sites ([`BucketRequest`]); the query
//!   itself never leaves the asking node (see [`crate::bucket`]). Each site
//!   comes with a [`RecordProof`] when the answering node holds a signed
//!   crawl of it. Asked under a throwaway identity, over a connection of
//!   its own.
//! * `/plumb/batch/1`: a batch by id, or the headers of the batches a node
//!   holds since an epoch, so a node that was away can catch up.
//! * Gossip topic `plumb/batches/1`: the [`SignedHeader`] of every new
//!   batch, as JSON. Nodes that want the batch fetch it with
//!   `/plumb/batch/1` from the node that passed the header on.
//! * `/plumb/report/1`: hands a popularity [`Report`] to a node, under a
//!   throwaway identity, or asks a node for the reports of a week it holds.
//! * Gossip topic `plumb/reports/1`: every popularity report a node is
//!   handed, as JSON, passed on by the node it was handed to.
//! * `/plumb/kad/1.0.0`: Kademlia, to find more nodes.
//!
//! Requests and responses are CBOR.

use serde::{Deserialize, Serialize};

use crate::batch::{Batch, RecordProof, SignedHeader};
use crate::hash::Hash;
use crate::popularity::Report;

pub const BUCKET_PROTOCOL: &str = "/plumb/bucket/1";
pub const BATCH_PROTOCOL: &str = "/plumb/batch/1";
pub const REPORT_PROTOCOL: &str = "/plumb/report/1";
pub const KAD_PROTOCOL: &str = "/plumb/kad/1.0.0";
pub const IDENTIFY_PROTOCOL: &str = "/plumb/id/1.0.0";
pub const BATCH_TOPIC: &str = "plumb/batches/1";
pub const REPORT_TOPIC: &str = "plumb/reports/1";

/// Most batch headers returned for one [`BatchRequest::List`].
pub const MAX_LISTED_BATCHES: usize = 10_000;
/// Most reports returned for one [`ReportRequest::List`].
pub const MAX_LISTED_REPORTS: usize = 50_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportRequest {
    /// Keep this report and pass it on.
    Submit(Report),
    /// The reports held of this report epoch.
    List { epoch: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportResponse {
    /// Whether a submitted report was taken (`false` for one already held
    /// or not valid).
    Taken(bool),
    Reports(Vec<Report>),
}

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
