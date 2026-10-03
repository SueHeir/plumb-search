//! Hiding who asks: bucket requests through a relay, the way Oblivious HTTP
//! (RFC 9458) works.
//!
//! A throwaway identity (see [`crate::search`]) already keeps the node
//! answering a bucket request from tying it to the asker's node id, but on
//! its own the node still sees the IP address the request came from. So a
//! node sends each bucket request through another node, the **relay**:
//!
//! 1. It asks the relay for the **target**'s key ([`ObliviousRequest::Keys`]).
//!    The relay fetches the key from the target over its own connection and
//!    keeps it for [`RELAY_KEY_CACHE`] seconds, handing everyone who asks
//!    the same one, so the target cannot give each asker a key of its own
//!    and recognize them by it. The key is signed with the target's node
//!    key, so the relay cannot swap in its own.
//! 2. It seals the bucket request to that key (HPKE, as in RFC 9458) and
//!    hands it to the relay ([`ObliviousRequest::Forward`]), which passes it
//!    to the target ([`ObliviousRequest::Deliver`]) and the sealed answer
//!    back.
//!
//! The relay sees the asker's IP address and which node it asks, but not
//! the bucket, and the answer only as ciphertext. The target sees the
//! bucket, but only the relay's address. Both are padded
//! ([`REQUEST_SIZE`], [`padded_len`]) so the relay, which holds much the
//! same buckets itself, cannot tell the bucket from the answer's size. Only
//! a relay and a target working together can link an IP to a bucket, and
//! the asker picks both at random for every request.
//!
//! The target's keys are made in memory and never written down, replaced
//! every [`KEY_LIFETIME`] seconds, the old one accepted until it expires.
//! What is sealed is a CBOR [`BucketRequest`] or
//! [`crate::proto::BucketResponse`] (the same messages as
//! `/plumb/bucket/1`), or a popularity [`Report`] padded to
//! [`REPORT_REQUEST_SIZE`] and its [`crate::proto::ReportResponse`], not
//! Binary HTTP: the encapsulation
//! is RFC 9458's, so the same code serves a browser going through an HTTP
//! relay, but the message inside is Plumb's own.
//!
//! The asker's half (checking keys, sealing, opening, padding) is
//! [`plumb_core::oblivious`], re-exported here, so the browser's WASM client
//! shares it without libp2p's networking.

use anyhow::{ensure, Context, Result};
use libp2p::identity::Keypair;
use libp2p::PeerId;
use ohttp::hpke::{Aead, Kdf, Kem};
use ohttp::{KeyConfig, Server, ServerResponse, SymmetricSuite};
pub use plumb_core::oblivious::{
    open_response, pad, padded_len, seal_request, seal_request_sized, unpad, ClientResponse,
    SignedKeys, KEY_LIFETIME, MAX_MESSAGE, MIN_RESPONSE_SIZE, REQUEST_SIZE,
};
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

use crate::popularity::Report;
use crate::proto::BucketRequest;

pub const OBLIVIOUS_PROTOCOL: &str = "/plumb/oblivious/1";
/// Seconds a relay hands out a target's key before fetching it again.
pub const RELAY_KEY_CACHE: u64 = 10 * 60;
/// Every sealed popularity [`Report`] has this size before encryption, so
/// the relay cannot tell one pick from another by its size, nor a report
/// from a bucket request ([`REQUEST_SIZE`]) by anything but its size.
pub const REPORT_REQUEST_SIZE: usize = 4 * 1024;
/// Every sealed bucket request that spends a token has this size, whatever
/// the bucket. The relay can tell it from a free one, not which bucket.
pub const PRIORITY_REQUEST_SIZE: usize = 256;

/// A sealed request, opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    Bucket(BucketRequest),
    /// A popularity report to keep and pass on; answered with a sealed
    /// [`crate::proto::ReportResponse`].
    Report(Report),
}

/// What goes over `/plumb/oblivious/1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObliviousRequest {
    /// To a relay: the key of `target`. A node answers for itself when it
    /// is the target.
    Keys { target: PeerId },
    /// To a relay: pass `message`, sealed to `target`'s key, on to it. A
    /// node answers it itself when it is the target.
    Forward { target: PeerId, message: ByteBuf },
    /// From a relay to the target: the receiver's own key.
    OwnKeys,
    /// From a relay to the target: a sealed request for the receiver. Never
    /// passed on again.
    Deliver { message: ByteBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObliviousResponse {
    /// The target's key, or `None` when it could not be had.
    Keys(Option<SignedKeys>),
    /// The sealed answer, or `None` when the request could not be passed
    /// on or answered.
    Sealed(Option<ByteBuf>),
}

fn suites() -> Vec<SymmetricSuite> {
    vec![
        SymmetricSuite::new(Kdf::HkdfSha256, Aead::Aes128Gcm),
        SymmetricSuite::new(Kdf::HkdfSha256, Aead::ChaCha20Poly1305),
    ]
}

/// A target's side: its keys, and opening the requests sealed to them.
pub struct Gateway {
    /// Newest first, at most two.
    keys: Vec<GatewayKey>,
}

struct GatewayKey {
    key_id: u8,
    server: Server,
    signed: SignedKeys,
    made: u64,
}

impl Gateway {
    /// A gateway with a fresh key, signed with `node_key`.
    pub fn new(node_key: &Keypair, now: u64) -> Result<Gateway> {
        let key = GatewayKey::new(node_key, now, None)?;
        Ok(Gateway { keys: vec![key] })
    }

    /// Makes a new key once the newest is [`KEY_LIFETIME`] old, and drops
    /// the expired ones.
    pub fn rotate(&mut self, node_key: &Keypair, now: u64) -> Result<()> {
        if now >= self.keys[0].made + KEY_LIFETIME {
            let previous = self.keys[0].key_id;
            let key = GatewayKey::new(node_key, now, Some(previous))?;
            self.keys.insert(0, key);
        }
        self.keys.truncate(2);
        self.keys.retain(|k| k.signed.expires > now);
        Ok(())
    }

    /// The key to seal new requests to.
    pub fn keys(&self) -> SignedKeys {
        self.keys[0].signed.clone()
    }

    /// Opens a sealed request. Returns it, and what seals the answer. Its
    /// size says which kind it is.
    pub fn open(&self, message: &[u8]) -> Result<(Opened, ServerResponse)> {
        ensure!(message.len() <= MAX_MESSAGE, "a sealed request too large");
        let key_id = *message.first().context("an empty sealed request")?;
        let key = self
            .keys
            .iter()
            .find(|k| k.key_id == key_id)
            .context("a request sealed to a key we no longer have")?;
        let (plain, opener) = key
            .server
            .decapsulate(message)
            .map_err(|err| anyhow::anyhow!("opening a sealed request: {err}"))?;
        let body = unpad(&plain)?;
        let opened = match plain.len() {
            REQUEST_SIZE | PRIORITY_REQUEST_SIZE => Opened::Bucket(
                cbor4ii::serde::from_slice(body).context("a sealed request that is not one")?,
            ),
            REPORT_REQUEST_SIZE => Opened::Report(
                cbor4ii::serde::from_slice(body).context("a sealed report that is not one")?,
            ),
            len => anyhow::bail!("a sealed request of {len} bytes"),
        };
        Ok((opened, opener))
    }
}

impl GatewayKey {
    fn new(node_key: &Keypair, now: u64, previous_id: Option<u8>) -> Result<GatewayKey> {
        let mut rng = rand_core::OsRng;
        let key_id = loop {
            let id = (rng.next_u32() & 0xff) as u8;
            if Some(id) != previous_id {
                break id;
            }
        };
        let config = KeyConfig::new(key_id, Kem::X25519Sha256, suites())
            .map_err(|err| anyhow::anyhow!("making a gateway key: {err}"))?;
        let encoded = config
            .encode()
            .map_err(|err| anyhow::anyhow!("encoding a gateway key: {err}"))?;
        let expires = now + 2 * KEY_LIFETIME;
        let signature = node_key
            .sign(&SignedKeys::signing_bytes(&encoded, expires))
            .context("signing a gateway key")?;
        let server = Server::new(config).map_err(|err| anyhow::anyhow!("a gateway key: {err}"))?;
        Ok(GatewayKey {
            key_id,
            server,
            signed: SignedKeys {
                node_key: ByteBuf::from(node_key.public().encode_protobuf()),
                config: ByteBuf::from(encoded),
                expires,
                signature: ByteBuf::from(signature),
            },
            made: now,
        })
    }
}

/// Seals the answer to an opened request.
pub fn seal_response<T: Serialize>(opener: ServerResponse, response: &T) -> Result<Vec<u8>> {
    let body = cbor4ii::serde::to_vec(Vec::new(), response).context("encoding an answer")?;
    let plain = pad(&body, MIN_RESPONSE_SIZE);
    ensure!(
        plain.len() <= MAX_MESSAGE - 1024,
        "an answer too large to seal"
    );
    opener
        .encapsulate(&plain)
        .map_err(|err| anyhow::anyhow!("sealing an answer: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::popularity::report_epoch;
    use crate::proto::{BucketRecord, BucketResponse, ReportResponse};

    const NOW: u64 = 1_790_000_000;

    #[test]
    fn a_request_goes_through_sealed_and_the_answer_comes_back() {
        let node = Keypair::generate_ed25519();
        let target = node.public().to_peer_id();
        let gateway = Gateway::new(&node, NOW).unwrap();
        let request = BucketRequest::new(1234);
        let (sealed, opener) = seal_request(&gateway.keys(), &target, NOW, &request).unwrap();
        // Every request is the same size, whatever the bucket.
        let (other, _) =
            seal_request(&gateway.keys(), &target, NOW, &BucketRequest::new(7)).unwrap();
        assert_eq!(sealed.len(), other.len());
        assert!(!sealed.windows(4).any(|w| w == 1234u32.to_be_bytes()));

        let (opened, sealer) = gateway.open(&sealed).unwrap();
        assert_eq!(opened, Opened::Bucket(request));
        let response = BucketResponse {
            records: Some(vec![BucketRecord {
                record: "{\"domain\":\"usbank.com\"}".into(),
                proof: None,
                also: Vec::new(),
            }]),
            busy: false,
        };
        let answer = seal_response(sealer, &response).unwrap();
        assert!(answer.len() >= MIN_RESPONSE_SIZE);
        assert_eq!(
            open_response::<BucketResponse>(opener, &answer).unwrap(),
            response
        );
    }

    #[test]
    fn keys_must_be_the_targets_and_current() {
        let node = Keypair::generate_ed25519();
        let target = node.public().to_peer_id();
        let keys = Gateway::new(&node, NOW).unwrap().keys();
        assert!(keys.verify(&target, NOW).is_ok());
        assert!(keys.verify(&target, NOW + 2 * KEY_LIFETIME).is_err());

        // A relay passing off its own key as the target's.
        let relay = Keypair::generate_ed25519();
        let swapped = Gateway::new(&relay, NOW).unwrap().keys();
        assert!(swapped.verify(&target, NOW).is_err());
        let mut forged = swapped.clone();
        forged.node_key = keys.node_key.clone();
        assert!(forged.verify(&target, NOW).is_err());
    }

    #[test]
    fn a_request_spending_a_token_opens_at_its_own_size() {
        use crate::credits::{Issuer, Pending};
        let node = Keypair::generate_ed25519();
        let target = node.public().to_peer_id();
        let gateway = Gateway::new(&node, NOW).unwrap();
        let issuer = Issuer::in_memory().unwrap();
        let pending = Pending::new(1).unwrap();
        let issued = issuer.issue(&pending.blinded).unwrap();
        let token = pending.finish(&issued).unwrap().remove(0);
        let request = BucketRequest {
            bucket: 9,
            token: Some(token),
        };
        let (sealed, _) = seal_request_sized(
            &gateway.keys(),
            &target,
            NOW,
            &request,
            PRIORITY_REQUEST_SIZE,
        )
        .unwrap();
        let (free, _) =
            seal_request(&gateway.keys(), &target, NOW, &BucketRequest::new(9)).unwrap();
        assert!(sealed.len() > free.len());
        assert_eq!(gateway.open(&sealed).unwrap().0, Opened::Bucket(request));
    }

    #[test]
    fn the_old_key_is_accepted_until_it_expires() {
        let node = Keypair::generate_ed25519();
        let target = node.public().to_peer_id();
        let mut gateway = Gateway::new(&node, NOW).unwrap();
        let old = gateway.keys();
        let request = BucketRequest::new(3);
        let (sealed, _) = seal_request(&old, &target, NOW, &request).unwrap();

        gateway.rotate(&node, NOW + KEY_LIFETIME).unwrap();
        assert_ne!(gateway.keys(), old);
        assert_eq!(gateway.open(&sealed).unwrap().0, Opened::Bucket(request));

        gateway.rotate(&node, NOW + 2 * KEY_LIFETIME).unwrap();
        assert!(gateway.open(&sealed).is_err());
    }

    #[test]
    fn a_report_goes_through_sealed_and_all_reports_are_one_size() {
        let node = Keypair::generate_ed25519();
        let target = node.public().to_peer_id();
        let gateway = Gateway::new(&node, NOW).unwrap();
        let epoch = report_epoch(NOW);
        let report = Report::new(epoch, "us bank", "usbank.com").unwrap();
        let seal = |report: &Report| {
            seal_request_sized(&gateway.keys(), &target, NOW, report, REPORT_REQUEST_SIZE).unwrap()
        };
        let (sealed, opener) = seal(&report);
        let longer = Report::new(epoch, "the national weather service", "weather.gov").unwrap();
        assert_eq!(sealed.len(), seal(&longer).0.len());

        let (opened, sealer) = gateway.open(&sealed).unwrap();
        assert_eq!(opened, Opened::Report(report));
        let answer = seal_response(sealer, &ReportResponse::Taken(true)).unwrap();
        assert_eq!(
            open_response::<ReportResponse>(opener, &answer).unwrap(),
            ReportResponse::Taken(true)
        );
    }
}
