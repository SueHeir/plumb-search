//! Bucket requests sealed to another node and sent through this site, so
//! that the site does not see the bucket and the node answering does not
//! see who asks (see `plumb_core::oblivious`, and `plumb-node`'s
//! `web/relay.rs` for the site's side).
//!
//! The messages are the network's own (`plumb_net::proto`'s
//! `BucketRequest` and `BucketResponse`), mirrored here because that crate
//! does not build for WebAssembly; `plumb-node`'s tests check the two agree.

use libp2p_identity::PeerId;
use plumb_core::oblivious::{open_response, seal_request, ClientResponse, SignedKeys};
use plumb_core::{canonical_domain, keys::slim_record, registrable_domain, SiteRecord};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

/// What `GET /api/oblivious/targets` lists.
#[derive(Debug, Clone, Deserialize)]
pub struct Targets {
    pub targets: Vec<Target>,
}

/// A node that answers bucket requests, and its signed key.
#[derive(Debug, Clone, Deserialize)]
pub struct Target {
    pub peer: String,
    pub keys: SignedKeys,
}

#[derive(Debug, Serialize)]
struct BucketRequest {
    bucket: u32,
}

#[derive(Debug, Deserialize)]
struct BucketResponse {
    records: Option<Vec<BucketRecord>>,
}

#[derive(Debug, Deserialize)]
struct BucketRecord {
    record: String,
    /// The node's proof of its crawl, which the browser does not check;
    /// see [`open`].
    #[serde(default)]
    #[allow(dead_code)]
    proof: Option<IgnoredAny>,
}

/// Seals a request for `bucket` to `target`, after checking its key is
/// signed by it and current at `now` (Unix seconds). Returns the sealed
/// request and what opens the answer.
pub fn seal(target: &Target, bucket: u32, now: u64) -> Result<(Vec<u8>, ClientResponse), String> {
    let peer: PeerId = target
        .peer
        .parse()
        .map_err(|_| "a node id that does not read".to_string())?;
    seal_request(&target.keys, &peer, now, &BucketRequest { bucket })
        .map_err(|err| format!("{err:#}"))
}

/// Opens a sealed answer: the bucket's sites, checked as a node checks
/// another node's (`plumb_net::search`): domains made canonical, and a URL
/// kept only when it is on the site's own domain, since without the crawl
/// proof (not checked here) a node could point a site anywhere.
pub fn open(opener: ClientResponse, answer: &[u8]) -> Result<Vec<SiteRecord>, String> {
    let response: BucketResponse =
        open_response(opener, answer).map_err(|err| format!("{err:#}"))?;
    let records = response.records.ok_or("that node has no buckets")?;
    Ok(records
        .into_iter()
        .filter_map(|item| serde_json::from_str::<SiteRecord>(&item.record).ok())
        .filter_map(|mut record| {
            record.domain = canonical_domain(&record.domain)?;
            if record
                .url
                .as_deref()
                .is_some_and(|url| registrable_domain(url).as_deref() != Some(&record.domain))
            {
                record.url = None;
            }
            Some(slim_record(record))
        })
        .collect())
}
