//! What nodes say to each other, and the names of the protocols they say it
//! over.
//!
//! * `/plumb/bucket/1`: one bucket of sites ([`BucketRequest`]); the query
//!   itself never leaves the asking node (see [`crate::bucket`]). Each site
//!   comes with a [`RecordProof`] when the answering node holds a signed
//!   crawl of it, and with proofs from other crawlers that agree with it
//!   when it holds those. Asked under a throwaway identity, over a connection of
//!   its own.
//! * `/plumb/oblivious/1`: the same bucket requests, sealed to the answering
//!   node's key and passed on by a relay, so the node answering never sees
//!   the asker's IP address (see [`crate::oblivious`]).
//! * `/plumb/batch/1`: a batch by id, or the headers of the batches a node
//!   holds since an epoch, so a node that was away can catch up.
//! * Gossip topic `plumb/batches/1`: the [`SignedHeader`] of every new
//!   batch, as JSON. Nodes that want the batch fetch it with
//!   `/plumb/batch/1` from the node that passed the header on.
//! * `/plumb/fill/1`: a stretch of a node's crawled sites, best-ranked
//!   first, for a node filling its free space, or every site for a node
//!   setting up (see [`crate::fill`]). Only taken from nodes the asker
//!   trusts.
//! * `/plumb/report/1`: hands a popularity [`Report`] to a node, under a
//!   throwaway identity, or asks a node for the reports of a week it holds.
//! * Gossip topic `plumb/reports/1`: every popularity report a node is
//!   handed, as JSON, passed on by the node it was handed to.
//! * `/plumb/credits/1`: asks a node for anonymous tokens paid with the
//!   credits it counts for the asker, or how many credits that is (see
//!   [`crate::credits`]). Asked over the asker's own identity, since its
//!   balance pays.
//! * `/plumb/trust/1`: which nodes a node trusts, asked by the nodes that
//!   trust it, so a search can ask friends of friends (see
//!   [`crate::scope`]).
//! * `/plumb/profile/1`: one searcher's profile (search history, About
//!   you, what their clicks taught), shared between the nodes they linked
//!   it on. Answered only for a profile linked with the asking node, so a
//!   node never learns anything of a profile it was not given. The
//!   connection is end to end encrypted, relayed or not.
//! * `/plumb/leads/1`: the newest [`Lead`]s a node holds, for a node it
//!   meets, so one that was away catches up (see [`crate::leads`]).
//! * Gossip topic `plumb/leads/1`: every lead a node shares, as JSON,
//!   passed on by every node that takes it.
//! * `/plumb/kad/1.0.0`: Kademlia, to find more nodes.
//!
//! Requests and responses are CBOR.

use serde::{Deserialize, Serialize};
pub use serde_bytes::ByteBuf;

use crate::batch::{Batch, RecordProof, SignedHeader};
use crate::credits::{Issued, Token};
use crate::hash::Hash;
use crate::leads::Lead;
use crate::popularity::Report;

pub const BUCKET_PROTOCOL: &str = "/plumb/bucket/1";
pub const BATCH_PROTOCOL: &str = "/plumb/batch/1";
pub const FILL_PROTOCOL: &str = "/plumb/fill/1";
pub const PAGES_PROTOCOL: &str = "/plumb/pages/1";
pub const TRUST_PROTOCOL: &str = "/plumb/trust/1";
pub const PROFILE_PROTOCOL: &str = "/plumb/profile/1";
pub const REPORT_PROTOCOL: &str = "/plumb/report/1";
pub const CREDIT_PROTOCOL: &str = "/plumb/credits/1";
pub const LEAD_PROTOCOL: &str = "/plumb/leads/1";
pub const KAD_PROTOCOL: &str = "/plumb/kad/1.0.0";
pub const IDENTIFY_PROTOCOL: &str = "/plumb/id/1.0.0";
pub const BATCH_TOPIC: &str = "plumb/batches/1";
pub const REPORT_TOPIC: &str = "plumb/reports/1";
pub const LEAD_TOPIC: &str = "plumb/leads/1";

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

/// Asks for the newest leads a node holds, at most
/// [`crate::leads::MAX_LISTED_LEADS`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeadRequest {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeadResponse {
    pub leads: Vec<Lead>,
}

/// Asks for one bucket (see [`crate::bucket`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketRequest {
    pub bucket: u32,
    /// A token the answering node issued (see [`crate::credits`]), spent
    /// to be answered when it is too busy to answer for free. Nodes that
    /// predate tokens ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<Token>,
}

impl BucketRequest {
    /// A free request for `bucket`.
    pub fn new(bucket: u32) -> BucketRequest {
        BucketRequest {
            bucket,
            token: None,
        }
    }
}

/// A bucket's records; `None` from a node that has no bucket table, or
/// that is too busy (`busy`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketResponse {
    pub records: Option<Vec<BucketRecord>>,
    /// Turned away for now: too many requests at once. A request with a
    /// token of the node's gets in.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub busy: bool,
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

/// Asks for the crawled sites among positions `from..` of the answering
/// node's list, best-ranked first (see [`crate::fill`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillRequest {
    pub from: u64,
    /// At most [`crate::fill::MAX_FILL_RECORDS`] are sent, or
    /// [`crate::fill::MAX_SEED_RECORDS`] with `all`.
    pub count: u32,
    /// Every site from `from` on, crawled or not, for a node setting up
    /// from the network instead of the seed downloads. Nodes from before
    /// it ignore it and send crawled sites only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillResponse {
    /// The crawled sites found (every site with `all`), as JSON,
    /// best-ranked first.
    pub records: Vec<String>,
    /// Where to ask from next; `total` once the list is done.
    pub next: u64,
    /// Sites in the answering node's list, crawled or not.
    pub total: u64,
    /// Turned away for now: it is filling others, or was asked too often.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub busy: bool,
}

/// Asks which nodes the answering node trusts (see [`crate::scope`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustRequest {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustResponse {
    /// Node ids, at most [`crate::scope::MAX_SHARED_TRUST`].
    pub trusted: Vec<String>,
}

/// Most bytes of a profile request or response.
pub const MAX_PROFILE_MESSAGE: u64 = 8 * 1024 * 1024;

/// About one searcher's profile, between two nodes they use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProfileRequest {
    /// Links the asker to the profile a link code was made for; `token` is
    /// the code's secret part, good once and only for a few minutes.
    Join { token: String },
    /// The asker's copy of `profile`, to merge with the answerer's. `round`
    /// names the copy both nodes agreed on last time, if any.
    Sync {
        profile: String,
        round: Option<u64>,
        state: ByteBuf,
    },
    /// The asker no longer shares `profile` with the answerer.
    Leave { profile: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProfileResponse {
    /// The profile linked, and the answerer's copy of it.
    Joined { profile: String, state: ByteBuf },
    /// The merged copy, which both nodes now keep as round `round`.
    Synced { round: u64, state: ByteBuf },
    /// Done (for [`ProfileRequest::Leave`]).
    Left,
    /// Not answered, and why.
    Refused(String),
}

/// Asks for `len` bytes from `offset` of the answering node's file of the
/// page set `set` (see [`crate::pages`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PagesRequest {
    pub set: String,
    pub offset: u64,
    /// At most [`crate::pages::MAX_PAGES_CHUNK`] are sent.
    pub len: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PagesResponse {
    /// The whole file's size; 0 when the node has no such set.
    pub size: u64,
    /// When the file was made, Unix seconds.
    pub modified: u64,
    pub bytes: ByteBuf,
    /// Turned away for now: it is serving others, or was asked too often.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub busy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreditRequest {
    /// Sign these blinded tokens, at most [`crate::credits::MAX_ISSUE`],
    /// paid from the asker's credits.
    Issue { blinded: Vec<ByteBuf> },
    /// The asker's credits, as the node asked counts them.
    Balance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreditResponse {
    /// As many of the tokens signed as the asker's credits pay for.
    Issued(Issued),
    /// The asker's balance, and whether its crawls count yet (a crawler
    /// gets tokens only once they do).
    Balance { credits: i64, counts: bool },
    /// No tokens, and why.
    Refused(String),
}
