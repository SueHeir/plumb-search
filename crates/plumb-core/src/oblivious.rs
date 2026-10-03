//! The asker's half of sealed bucket requests: checking a node's key,
//! sealing a request to it, opening the answer, and the padding both sides
//! use. Shared by nodes (`plumb-net`, which adds the relay and the answering
//! side) and the browser's WASM client, which is why it lives here and
//! needs nothing from libp2p beyond node keys.
//!
//! How the whole exchange works is in `plumb_net::oblivious`. In short: the
//! request goes through a relay, sealed with RFC 9458's encapsulation (HPKE)
//! to a key the answering node signed, so the relay never sees what is asked
//! and the answering node never sees who asks. What is sealed is any
//! serde value as CBOR, length-prefixed and padded, not Binary HTTP.
//!
//! Building for `wasm32-unknown-unknown` needs `getrandom` 0.4 with its
//! `wasm_js` feature turned on in the WASM crate.

use anyhow::{bail, ensure, Context, Result};
use libp2p_identity::{PeerId, PublicKey};
use ohttp::ClientRequest;
pub use ohttp::ClientResponse;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

/// Seconds a node's key is used for new requests; it is still accepted for
/// as long again.
pub const KEY_LIFETIME: u64 = 24 * 60 * 60;
/// Every sealed request has this size before encryption.
pub const REQUEST_SIZE: usize = 64;
/// The smallest sealed answer, before encryption.
pub const MIN_RESPONSE_SIZE: usize = 4 * 1024;
/// The largest sealed message a node takes, either way.
pub const MAX_MESSAGE: usize = 48 * 1024 * 1024;

const SIGNING_CONTEXT: &[u8] = b"plumb-oblivious-keys-v1\0";

/// A node's key config (RFC 9458 section 3), signed with its node key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedKeys {
    /// The node's key, protobuf-encoded.
    pub node_key: ByteBuf,
    /// The key config, as in `application/ohttp-keys`.
    pub config: ByteBuf,
    /// Unix time after which the key is not accepted.
    pub expires: u64,
    pub signature: ByteBuf,
}

impl SignedKeys {
    /// What the node signs.
    pub fn signing_bytes(config: &[u8], expires: u64) -> Vec<u8> {
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

/// Seals `request` to `target`'s `keys`, after checking them. Returns the
/// sealed request, and what opens the answer.
pub fn seal_request<T: Serialize>(
    keys: &SignedKeys,
    target: &PeerId,
    now: u64,
    request: &T,
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
pub fn open_response<T: DeserializeOwned>(opener: ClientResponse, message: &[u8]) -> Result<T> {
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

/// `body` with its length in front, padded with zeros to
/// [`padded_len`]`(len + 4, min)`.
pub fn pad(body: &[u8], min: usize) -> Vec<u8> {
    let size = padded_len(body.len() + 4, min);
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    out.resize(size, 0);
    out
}

/// The body of a message made by [`pad`].
pub fn unpad(plain: &[u8]) -> Result<&[u8]> {
    let Some((len, rest)) = plain.split_first_chunk::<4>() else {
        bail!("a sealed message too short");
    };
    let len = u32::from_be_bytes(*len) as usize;
    rest.get(..len).context("a sealed message cut short")
}

#[cfg(test)]
mod tests {
    use super::*;

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
