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
//! What is sealed is a CBOR [`BucketRequest`] or [`BucketResponse`] (the
//! same messages as `/plumb/bucket/1`), not Binary HTTP: the encapsulation
//! is RFC 9458's, so the same code serves a browser going through an HTTP
//! relay, but the message inside is Plumb's own.

use anyhow::{bail, ensure, Context, Result};
use libp2p::identity::{Keypair, PublicKey};
use libp2p::PeerId;
use ohttp::hpke::{Aead, Kdf, Kem};
use ohttp::{ClientRequest, ClientResponse, KeyConfig, Server, ServerResponse, SymmetricSuite};
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

use crate::proto::{BucketRequest, BucketResponse};

pub const OBLIVIOUS_PROTOCOL: &str = "/plumb/oblivious/1";
/// Seconds a target's key is used for new requests; it is still accepted
/// for as long again.
pub const KEY_LIFETIME: u64 = 24 * 60 * 60;
/// Seconds a relay hands out a target's key before fetching it again.
pub const RELAY_KEY_CACHE: u64 = 10 * 60;
/// Every sealed bucket request has this size before encryption.
pub const REQUEST_SIZE: usize = 64;
/// The smallest sealed answer, before encryption.
pub const MIN_RESPONSE_SIZE: usize = 4 * 1024;
/// The largest sealed message a node takes, either way.
pub const MAX_MESSAGE: usize = 48 * 1024 * 1024;

const SIGNING_CONTEXT: &[u8] = b"plumb-oblivious-keys-v1\0";

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

/// A target's key config (RFC 9458 section 3), signed with its node key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedKeys {
    /// The target's node key, protobuf-encoded.
    pub node_key: ByteBuf,
    /// The key config, as in `application/ohttp-keys`.
    pub config: ByteBuf,
    /// Unix time after which the key is not accepted.
    pub expires: u64,
    pub signature: ByteBuf,
}

impl SignedKeys {
    fn signing_bytes(config: &[u8], expires: u64) -> Vec<u8> {
        let mut bytes = SIGNING_CONTEXT.to_vec();
        bytes.extend_from_slice(&expires.to_be_bytes());
        bytes.extend_from_slice(config);
        bytes
    }

    /// Checks that these are `target`'s keys, signed by it, and not
    /// expired at `now`.
    pub fn verify(&self, target: &PeerId, now: u64) -> Result<()> {
        let key = PublicKey::try_decode_protobuf(&self.node_key).context("not a node key")?;
        ensure!(key.to_peer_id() == *target, "the keys of another node");
        ensure!(
            key.verify(
                &Self::signing_bytes(&self.config, self.expires),
                &self.signature
            ),
            "a bad signature"
        );
        ensure!(self.expires > now, "expired keys");
        ensure!(
            self.expires <= now + 2 * KEY_LIFETIME + 3600,
            "keys that last too long"
        );
        Ok(())
    }
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

    /// Opens a sealed request. Returns it, and what seals the answer.
    pub fn open(&self, message: &[u8]) -> Result<(BucketRequest, ServerResponse)> {
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
        let request: BucketRequest =
            cbor4ii::serde::from_slice(body).context("a sealed request that is not one")?;
        Ok((request, opener))
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
pub fn seal_response(opener: ServerResponse, response: &BucketResponse) -> Result<Vec<u8>> {
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

/// Seals a request for `bucket` to `target`'s `keys`, after checking them.
/// Returns the sealed request, and what opens the answer.
pub fn seal_request(
    keys: &SignedKeys,
    target: &PeerId,
    now: u64,
    request: &BucketRequest,
) -> Result<(Vec<u8>, ClientResponse)> {
    keys.verify(target, now)?;
    let body = cbor4ii::serde::to_vec(Vec::new(), request).context("encoding a request")?;
    let plain = pad(&body, REQUEST_SIZE);
    ensure!(plain.len() == REQUEST_SIZE, "a request too large");
    ClientRequest::from_encoded_config(&keys.config)
        .and_then(|client| client.encapsulate(&plain))
        .map_err(|err| anyhow::anyhow!("sealing a request: {err}"))
}

/// Opens a sealed answer.
pub fn open_response(opener: ClientResponse, message: &[u8]) -> Result<BucketResponse> {
    ensure!(message.len() <= MAX_MESSAGE, "a sealed answer too large");
    let plain = opener
        .decapsulate(message)
        .map_err(|err| anyhow::anyhow!("opening a sealed answer: {err}"))?;
    cbor4ii::serde::from_slice(unpad(&plain)?).context("a sealed answer that is not one")
}

/// The size `len` bytes are padded to: at least `min`, and otherwise the
/// next of 2^k and 1.5 * 2^k, so a message grows by at most half and its
/// size says little about what is in it.
pub fn padded_len(len: usize, min: usize) -> usize {
    if len <= min {
        return min;
    }
    let mut size = min.next_power_of_two();
    loop {
        if size >= len {
            return size;
        }
        if size + size / 2 >= len {
            return size + size / 2;
        }
        size *= 2;
    }
}

/// `body` with its length in front, padded with zeros.
fn pad(body: &[u8], min: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(padded_len(body.len() + 4, min));
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    out.resize(padded_len(body.len() + 4, min), 0);
    out
}

fn unpad(plain: &[u8]) -> Result<&[u8]> {
    let Some((len, rest)) = plain.split_first_chunk::<4>() else {
        bail!("a sealed message too short");
    };
    let len = u32::from_be_bytes(*len) as usize;
    rest.get(..len).context("a sealed message cut short")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::BucketRecord;

    const NOW: u64 = 1_790_000_000;

    #[test]
    fn a_request_goes_through_sealed_and_the_answer_comes_back() {
        let node = Keypair::generate_ed25519();
        let target = node.public().to_peer_id();
        let gateway = Gateway::new(&node, NOW).unwrap();
        let request = BucketRequest { bucket: 1234 };
        let (sealed, opener) = seal_request(&gateway.keys(), &target, NOW, &request).unwrap();
        // Every request is the same size, whatever the bucket.
        let (other, _) =
            seal_request(&gateway.keys(), &target, NOW, &BucketRequest { bucket: 7 }).unwrap();
        assert_eq!(sealed.len(), other.len());
        assert!(!sealed.windows(4).any(|w| w == 1234u32.to_be_bytes()));

        let (opened, sealer) = gateway.open(&sealed).unwrap();
        assert_eq!(opened, request);
        let response = BucketResponse {
            records: Some(vec![BucketRecord {
                record: "{\"domain\":\"usbank.com\"}".into(),
                proof: None,
            }]),
        };
        let answer = seal_response(sealer, &response).unwrap();
        assert!(answer.len() >= MIN_RESPONSE_SIZE);
        assert_eq!(open_response(opener, &answer).unwrap(), response);
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
    fn the_old_key_is_accepted_until_it_expires() {
        let node = Keypair::generate_ed25519();
        let target = node.public().to_peer_id();
        let mut gateway = Gateway::new(&node, NOW).unwrap();
        let old = gateway.keys();
        let request = BucketRequest { bucket: 3 };
        let (sealed, _) = seal_request(&old, &target, NOW, &request).unwrap();

        gateway.rotate(&node, NOW + KEY_LIFETIME).unwrap();
        assert_ne!(gateway.keys(), old);
        assert_eq!(gateway.open(&sealed).unwrap().0, request);

        gateway.rotate(&node, NOW + 2 * KEY_LIFETIME).unwrap();
        assert!(gateway.open(&sealed).is_err());
    }

    #[test]
    fn padding_grows_by_at_most_half() {
        assert_eq!(padded_len(10, 64), 64);
        assert_eq!(padded_len(4096, 4096), 4096);
        assert_eq!(padded_len(4097, 4096), 6144);
        assert_eq!(padded_len(6145, 4096), 8192);
        assert_eq!(padded_len(100_000, 4096), 131_072);
        for len in [5000, 70_000, 1_000_000] {
            let padded = padded_len(len, 4096);
            assert!(
                padded >= len && padded <= len + len / 2 + 1,
                "{len} {padded}"
            );
        }
        let body = b"hello";
        assert_eq!(unpad(&pad(body, 64)).unwrap(), body);
        assert!(unpad(&[0, 0, 0, 9, 1]).is_err());
    }
}
