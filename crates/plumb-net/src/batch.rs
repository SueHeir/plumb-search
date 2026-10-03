//! Crawl batches: the records one node's crawl produced, signed by that
//! node, so that any other node can check them whoever it got them from.
//!
//! A batch is a list of site records, each kept as the exact JSON bytes that
//! were hashed, and a [`SignedHeader`]: the crawler's public key, the epoch
//! the crawl was assigned in, the number of records and the Merkle root over
//! them, signed with the crawler's key. Because the header commits to every
//! record, a node can relay a batch it did not make, and a search answer can
//! carry one record with a [`RecordProof`] instead of the whole batch.
//!
//! A valid signature only says who sent a record, not that it is true. So
//! a receiving node keeps only what a crawl of the assigned sites could have
//! seen ([`accept_batch`]): homepage facts (URL, title, description, site
//! name) for sites the crawler was assigned that epoch, and link text and
//! new domains found on those homepages. Popularity ranks, Wikidata status
//! and crawl bookkeeping are never taken from another node.

use std::collections::HashSet;

use anyhow::{bail, ensure, Context, Result};
use libp2p::identity::{Keypair, PublicKey};
use libp2p::PeerId;
use plumb_core::{canonical_domain, linker_bit, registrable_domain, SiteRecord};
use serde::{Deserialize, Serialize};

use crate::assign::{epoch_of, is_assigned, EPOCH_SECS, MAX_SHARE_PPM};
use crate::hash::{leaf_hash, merkle_root, Hash, MerkleProof};

/// Most records in one batch.
pub const MAX_BATCH_RECORDS: usize = 4_096;

/// Longest record, as JSON, accepted from another node.
pub const MAX_RECORD_BYTES: usize = 16 * 1024;

/// Batches from epochs older than this many days are not accepted.
pub const MAX_BATCH_AGE_EPOCHS: u64 = 7;

/// Linked sites a batch may name per homepage it crawled; a homepage with
/// more links than this is rare, and the cap keeps one batch from flooding
/// the network with made-up domains.
pub const MAX_LINKED_PER_CRAWLED: usize = 100;

const SIGNING_CONTEXT: &[u8] = b"plumb-batch-v1\0";

/// What a crawler signs about a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchHeader {
    /// The crawler's public key, protobuf-encoded as libp2p does.
    #[serde(with = "bytes_hex")]
    pub crawler: Vec<u8>,
    /// The epoch whose site assignment the crawl followed.
    pub epoch: u64,
    /// The crawler's share of sites that epoch, in parts per million.
    pub share_ppm: u32,
    /// When the batch was made, in Unix seconds.
    pub created_at: u64,
    /// Number of records.
    pub count: u32,
    /// Merkle root over the records' leaf hashes.
    pub root: Hash,
}

impl BatchHeader {
    /// The bytes signed: fixed-width fields after a context string, so a
    /// signature over a batch header can never be mistaken for any other.
    fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SIGNING_CONTEXT.len() + 4 + self.crawler.len() + 60);
        out.extend_from_slice(SIGNING_CONTEXT);
        out.extend_from_slice(&(self.crawler.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.crawler);
        out.extend_from_slice(&self.epoch.to_be_bytes());
        out.extend_from_slice(&self.share_ppm.to_be_bytes());
        out.extend_from_slice(&self.created_at.to_be_bytes());
        out.extend_from_slice(&self.count.to_be_bytes());
        out.extend_from_slice(&self.root.0);
        out
    }
}

/// A batch header and the crawler's signature of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedHeader {
    pub header: BatchHeader,
    #[serde(with = "bytes_hex")]
    pub signature: Vec<u8>,
}

impl SignedHeader {
    /// Identifies the batch: the hash of what was signed.
    pub fn id(&self) -> Hash {
        Hash::of(&[&self.header.signing_bytes()])
    }

    /// The crawler's key and id, once the signature checks out.
    pub fn verify(&self) -> Result<(PublicKey, PeerId)> {
        let key = PublicKey::try_decode_protobuf(&self.header.crawler)
            .context("the crawler key does not decode")?;
        ensure!(
            key.verify(&self.header.signing_bytes(), &self.signature),
            "the batch signature does not match"
        );
        let peer = key.to_peer_id();
        Ok((key, peer))
    }

    /// Checks the signature, and that the batch is recent, not from the
    /// future, not too big and within the share cap. Returns the crawler.
    pub fn check(&self, now: u64) -> Result<PeerId> {
        let (_, crawler) = self.verify()?;
        let h = &self.header;
        let current = epoch_of(now);
        ensure!(
            h.epoch <= current + 1 && h.epoch + MAX_BATCH_AGE_EPOCHS >= current,
            "the batch is from epoch {}, too far from the current {current}",
            h.epoch
        );
        ensure!(
            h.created_at <= now + EPOCH_SECS / 24,
            "the batch was made in the future"
        );
        ensure!(
            h.count as usize <= MAX_BATCH_RECORDS && h.count > 0,
            "a batch holds 1 to {MAX_BATCH_RECORDS} records, this one {}",
            h.count
        );
        ensure!(
            h.share_ppm <= MAX_SHARE_PPM,
            "the crawler claims a share of {} ppm, above the cap of {MAX_SHARE_PPM}",
            h.share_ppm
        );
        Ok(crawler)
    }
}

/// A signed batch with its records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
    pub header: SignedHeader,
    /// Each record as the JSON that was hashed.
    pub records: Vec<String>,
}

impl Batch {
    /// Signs `records` with `key`. Records with invalid domains are left
    /// out, and at most [`MAX_BATCH_RECORDS`] are kept; `None` when none are.
    pub fn sign(
        key: &Keypair,
        records: &[SiteRecord],
        epoch: u64,
        share_ppm: u32,
        now: u64,
    ) -> Result<Option<Batch>> {
        let mut lines = Vec::new();
        for record in records.iter().take(MAX_BATCH_RECORDS) {
            if canonical_domain(&record.domain).as_deref() != Some(record.domain.as_str()) {
                continue;
            }
            lines.push(serde_json::to_string(record).context("encoding a record")?);
        }
        if lines.is_empty() {
            return Ok(None);
        }
        let leaves: Vec<Hash> = lines.iter().map(|l| leaf_hash(l.as_bytes())).collect();
        let header = BatchHeader {
            crawler: key.public().encode_protobuf(),
            epoch,
            share_ppm: share_ppm.min(MAX_SHARE_PPM),
            created_at: now,
            count: lines.len() as u32,
            root: merkle_root(&leaves),
        };
        let signature = key
            .sign(&header.signing_bytes())
            .context("signing the batch")?;
        Ok(Some(Batch {
            header: SignedHeader { header, signature },
            records: lines,
        }))
    }

    pub fn id(&self) -> Hash {
        self.header.id()
    }

    fn leaves(&self) -> Vec<Hash> {
        self.records
            .iter()
            .map(|l| leaf_hash(l.as_bytes()))
            .collect()
    }

    /// Checks the header ([`SignedHeader::check`]) and that the records are
    /// the ones it commits to. Returns the crawler.
    pub fn check(&self, now: u64) -> Result<PeerId> {
        let crawler = self.header.check(now)?;
        let h = &self.header.header;
        ensure!(
            self.records.len() == h.count as usize,
            "the header counts {} records, the batch holds {}",
            h.count,
            self.records.len()
        );
        ensure!(
            merkle_root(&self.leaves()) == h.root,
            "the records do not match the signed root"
        );
        Ok(crawler)
    }

    /// The proof that record `index` is in this batch.
    pub fn proof(&self, index: usize) -> RecordProof {
        RecordProof {
            header: self.header.clone(),
            record: self.records[index].clone(),
            path: MerkleProof::new(&self.leaves(), index),
        }
    }
}

/// One record of a signed batch, with what it takes to check it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordProof {
    pub header: SignedHeader,
    /// The record as the JSON that was hashed.
    pub record: String,
    pub path: MerkleProof,
}

impl RecordProof {
    /// Checks the signature and Merkle path, and that the record is a
    /// homepage crawl of a site the crawler was assigned. Returns the
    /// record as [`accept_batch`] would keep it, and the crawler.
    pub fn verify(&self, now: u64) -> Result<(SiteRecord, PeerId)> {
        let crawler = self.header.check(now)?;
        let h = &self.header.header;
        ensure!(
            self.path
                .verify(leaf_hash(self.record.as_bytes()), h.count, &h.root),
            "the record is not in the signed batch"
        );
        let record = parse_record(&self.record)?;
        let Some(record) = accept_crawled(record, &crawler, h, now, true) else {
            bail!("the record is not a homepage crawl the crawler was assigned");
        };
        Ok((record, crawler))
    }
}

/// What a node keeps of a batch from another node, once [`Batch::check`]
/// has passed: the records a crawl of its assigned sites could have
/// produced, cut down to the fields a crawl can see.
///
/// * A crawled homepage (a record with `crawled_at`) is kept only for sites
///   the crawler was assigned in the batch's epoch, with a URL on the site
///   and a crawl time inside that epoch: URL, title, description, aliases
///   and crawl time.
/// * Link text is kept only when every linking site it names is one of
///   the batch's crawled homepages, and a count of linking sites is capped
///   at how many homepages the batch crawled.
/// * Other sites the homepages linked to are kept as bare names (plus that
///   link text), at most [`MAX_LINKED_PER_CRAWLED`] per crawled homepage.
///
/// Everything else (ranks, Wikidata status, crawl attempts and failures) is
/// dropped: those come from public seed data or local bookkeeping.
pub fn accept_batch(batch: &Batch, crawler: &PeerId, now: u64) -> Vec<SiteRecord> {
    accept(batch, crawler, now, true)
}

/// What a node takes from a batch it signed itself: the same as
/// [`accept_batch`], except that its homepages need not be ones it was
/// assigned, since a node may fetch a disputed site to settle it (see
/// [`crate::agree`]). Other nodes still ignore those.
pub fn accept_own_batch(batch: &Batch, me: &PeerId, now: u64) -> Vec<SiteRecord> {
    accept(batch, me, now, false)
}

fn accept(batch: &Batch, crawler: &PeerId, now: u64, assigned_only: bool) -> Vec<SiteRecord> {
    let h = &batch.header.header;
    let parsed: Vec<SiteRecord> = batch
        .records
        .iter()
        .filter_map(|line| parse_record(line).ok())
        .collect();
    // Crawled homepages, each with the links other homepages of the batch
    // made to it, and the rest.
    let mut crawled = Vec::new();
    let mut others = Vec::new();
    for record in parsed {
        if record.crawled_at.is_some() {
            let links = (record.link_texts.clone(), record.signals.linking_domains);
            if let Some(kept) = accept_crawled(record, crawler, h, now, assigned_only) {
                crawled.push((kept, links));
            }
        } else {
            others.push(record);
        }
    }
    let crawled_bits: u64 = crawled
        .iter()
        .fold(0, |bits, (r, _)| bits | linker_bit(&r.domain));
    let crawled_domains: HashSet<String> = crawled.iter().map(|(r, _)| r.domain.clone()).collect();
    let max_linkers = crawled.len() as u32;
    let cap = crawled.len() * MAX_LINKED_PER_CRAWLED;
    let mut kept: Vec<SiteRecord> = Vec::with_capacity(crawled.len());
    for (mut record, (texts, linking)) in crawled {
        keep_links(&mut record, &texts, linking, crawled_bits, max_linkers);
        kept.push(record);
    }
    let mut linked = 0;
    for record in others {
        if linked >= cap {
            break;
        }
        if crawled_domains.contains(&record.domain) {
            continue;
        }
        let mut site = SiteRecord::new(record.domain);
        keep_links(
            &mut site,
            &record.link_texts,
            record.signals.linking_domains,
            crawled_bits,
            max_linkers,
        );
        kept.push(site);
        linked += 1;
    }
    kept
}

/// Reads one record from another node, with its domain made canonical.
fn parse_record(line: &str) -> Result<SiteRecord> {
    ensure!(line.len() <= MAX_RECORD_BYTES, "a record is too long");
    let mut record: SiteRecord = serde_json::from_str(line).context("a record does not parse")?;
    let Some(domain) = canonical_domain(&record.domain) else {
        bail!("{:?} is not a registrable domain", record.domain);
    };
    record.domain = domain;
    Ok(record)
}

/// A crawled homepage from another node, cut down to what a crawl sees, or
/// `None` when it was not the crawler's to crawl.
fn accept_crawled(
    record: SiteRecord,
    crawler: &PeerId,
    header: &BatchHeader,
    now: u64,
    assigned_only: bool,
) -> Option<SiteRecord> {
    let crawled_at = record.crawled_at?;
    let epoch_start = header.epoch * EPOCH_SECS;
    // A crawl assigned in an epoch may run on a little past its end.
    let in_epoch = crawled_at >= epoch_start
        && crawled_at < epoch_start + 2 * EPOCH_SECS
        && crawled_at <= now + EPOCH_SECS / 24;
    let assigned = || is_assigned(header.epoch, crawler, &record.domain, header.share_ppm);
    if !in_epoch || (assigned_only && !assigned()) {
        return None;
    }
    let url = record
        .url
        .filter(|url| registrable_domain(url).as_deref() == Some(record.domain.as_str()));
    let mut kept = SiteRecord::new(record.domain);
    kept.url = url;
    kept.title = record.title;
    kept.description = record.description;
    for alias in &record.aliases {
        kept.add_alias(alias);
    }
    kept.crawled_at = Some(crawled_at);
    Some(kept)
}

fn keep_links(
    kept: &mut SiteRecord,
    texts: &[plumb_core::LinkText],
    linking_domains: u32,
    crawled_bits: u64,
    max_linkers: u32,
) {
    for lt in texts {
        if lt.linkers != 0 && lt.linkers & !crawled_bits == 0 {
            kept.add_link_text_linkers(&lt.text, lt.linkers);
        }
    }
    kept.signals.linking_domains = linking_domains.min(max_linkers);
}

/// Bytes as lowercase hex in JSON and CBOR.
pub(crate) mod bytes_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            out.push_str(&format!("{byte:02x}"));
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        if s.len() % 2 != 0 || s.len() > 8_192 {
            return Err(serde::de::Error::custom("bad hex length"));
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(serde::de::Error::custom))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use plumb_core::LinkText;

    use super::*;

    const NOW: u64 = 1_790_000_000;

    fn assigned_domains(peer: &PeerId, n: usize) -> Vec<String> {
        (0..)
            .map(|i| format!("site{i}.com"))
            .filter(|d| is_assigned(epoch_of(NOW), peer, d, MAX_SHARE_PPM))
            .take(n)
            .collect()
    }

    fn crawled(domain: &str) -> SiteRecord {
        let mut r = SiteRecord::new(domain);
        r.url = Some(format!("https://www.{domain}/"));
        r.title = Some(format!("{domain} home"));
        r.crawled_at = Some(NOW - 60);
        r.signals.tranco_rank = Some(1);
        r.signals.official_site = true;
        r.crawl_failures = 3;
        r
    }

    fn sign(key: &Keypair, records: &[SiteRecord]) -> Batch {
        Batch::sign(key, records, epoch_of(NOW), MAX_SHARE_PPM, NOW)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn a_signed_batch_checks_out_and_survives_json() {
        let key = Keypair::generate_ed25519();
        let peer = key.public().to_peer_id();
        let records: Vec<_> = assigned_domains(&peer, 5)
            .iter()
            .map(|d| crawled(d))
            .collect();
        let batch = sign(&key, &records);
        let json = serde_json::to_string(&batch).unwrap();
        let back: Batch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.check(NOW).unwrap(), peer);
        assert_eq!(back.id(), batch.id());
    }

    #[test]
    fn tampering_with_a_record_or_the_header_is_caught() {
        let key = Keypair::generate_ed25519();
        let peer = key.public().to_peer_id();
        let records: Vec<_> = assigned_domains(&peer, 3)
            .iter()
            .map(|d| crawled(d))
            .collect();
        let batch = sign(&key, &records);

        let mut changed = batch.clone();
        changed.records[1] = changed.records[1].replace("home", "phish");
        assert!(changed.check(NOW).is_err());

        let mut reepoched = batch.clone();
        reepoched.header.header.epoch -= 1;
        assert!(reepoched.check(NOW).is_err());

        let mut resigned = batch.clone();
        let other = Keypair::generate_ed25519();
        resigned.header.header.crawler = other.public().encode_protobuf();
        assert!(resigned.check(NOW).is_err());

        assert!(batch.check(NOW + 30 * EPOCH_SECS).is_err(), "too old");
    }

    #[test]
    fn only_assigned_homepages_and_crawl_facts_are_accepted() {
        let key = Keypair::generate_ed25519();
        let peer = key.public().to_peer_id();
        let mine = assigned_domains(&peer, 2);
        let not_mine = (0..)
            .map(|i| format!("other{i}.org"))
            .find(|d| !is_assigned(epoch_of(NOW), &peer, d, MAX_SHARE_PPM))
            .unwrap();
        let records = vec![crawled(&mine[0]), crawled(&mine[1]), crawled(&not_mine)];
        let batch = sign(&key, &records);
        let kept = accept_batch(&batch, &batch.check(NOW).unwrap(), NOW);
        let domains: Vec<_> = kept.iter().map(|r| r.domain.as_str()).collect();
        assert_eq!(domains, vec![mine[0].as_str(), mine[1].as_str()]);
        let first = &kept[0];
        assert_eq!(
            first.title.as_deref(),
            Some(format!("{} home", mine[0]).as_str())
        );
        assert_eq!(
            first.signals,
            Default::default(),
            "ranks are never taken from peers"
        );
        assert_eq!(first.crawl_failures, 0);
        // A node's own batch keeps the site it fetched to settle a dispute.
        let own = accept_own_batch(&batch, &peer, NOW);
        assert_eq!(own.len(), 3);
        assert_eq!(own[2].domain, not_mine);
    }

    #[test]
    fn link_text_must_come_from_the_batch_own_homepages() {
        let key = Keypair::generate_ed25519();
        let peer = key.public().to_peer_id();
        let mine = assigned_domains(&peer, 1);
        let mut target = SiteRecord::new("usbank.com");
        target.link_texts = vec![
            LinkText::from_linkers("us bank", linker_bit(&mine[0])),
            LinkText::from_linkers("free money", linker_bit("someone-else-entirely.net") | 1),
        ];
        target.signals.linking_domains = 500;
        let batch = sign(&key, &[crawled(&mine[0]), target]);
        let kept = accept_batch(&batch, &peer, NOW);
        let usbank = kept.iter().find(|r| r.domain == "usbank.com").unwrap();
        let texts: Vec<_> = usbank
            .link_texts
            .iter()
            .map(|lt| lt.text.as_str())
            .collect();
        // The second text claims a linker bit the batch's homepages lack,
        // unless the hash happens to collide; both were OR-ed with bit 0 too.
        if linker_bit(&mine[0]) != 1 {
            assert_eq!(texts, vec!["us bank"]);
        }
        assert_eq!(usbank.signals.linking_domains, 1);
    }

    #[test]
    fn a_batch_with_no_valid_homepage_names_no_other_sites() {
        let key = Keypair::generate_ed25519();
        let peer = key.public().to_peer_id();
        let batch = sign(
            &key,
            &[SiteRecord::new("spam1.com"), SiteRecord::new("spam2.com")],
        );
        assert!(accept_batch(&batch, &peer, NOW).is_empty());
    }

    #[test]
    fn a_record_proof_verifies_alone() {
        let key = Keypair::generate_ed25519();
        let peer = key.public().to_peer_id();
        let records: Vec<_> = assigned_domains(&peer, 9)
            .iter()
            .map(|d| crawled(d))
            .collect();
        let batch = sign(&key, &records);
        let proof = batch.proof(4);
        let (record, crawler) = proof.verify(NOW).unwrap();
        assert_eq!(crawler, peer);
        assert_eq!(record.domain, records[4].domain);

        let mut forged = proof.clone();
        forged.record = forged.record.replace("home", "login");
        assert!(forged.verify(NOW).is_err());
        let mut moved = proof;
        moved.path.index = 5;
        assert!(moved.verify(NOW).is_err());
    }
}
