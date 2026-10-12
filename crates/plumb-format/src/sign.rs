//! Signing and checking set manifests.
//!
//! The trust is in the file, not in whoever sent it: a node checks a
//! manifest's signature against the publisher's key, and each part's hash
//! against the manifest, so any node can pass a set along.
//!
//! A manifest is signed with Ed25519 over the bytes `plumb-set/1\0`
//! followed by its canonical JSON ([`crate::canonical`]) without its
//! `signature` key. The leading words keep a set signature from ever
//! passing for another kind of signature, such as a crawl batch's
//! (`plumb-batch-v1\0`). Key notes ([`crate::notes`]) are signed the same
//! way under `plumb-note/1\0`, so a note never passes for a manifest nor a
//! manifest for a note. Keys and signatures are written in unpadded
//! base64url, keys as `ed25519:<key>`.

use std::fmt;
use std::str::FromStr;

use anyhow::{bail, ensure, Context, Error, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::canonical::canonical_json;

/// What every set signature covers before the manifest itself.
pub const SET_CONTEXT: &[u8] = b"plumb-set/1\0";

/// What every key note's signature covers before the note itself.
pub const NOTE_CONTEXT: &[u8] = b"plumb-note/1\0";

/// A publisher's or signer's Ed25519 public key.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey([u8; 32]);

impl PublicKey {
    /// The public half of `key`.
    pub fn of(key: &SigningKey) -> PublicKey {
        PublicKey(key.verifying_key().to_bytes())
    }

    fn verifying_key(&self) -> Result<VerifyingKey> {
        VerifyingKey::from_bytes(&self.0).context("not an Ed25519 public key")
    }
}

impl FromStr for PublicKey {
    type Err = Error;

    fn from_str(text: &str) -> Result<PublicKey> {
        let encoded = text
            .strip_prefix("ed25519:")
            .with_context(|| format!("key {text:?} is not ed25519:<base64url>"))?;
        let bytes: [u8; 32] = URL_SAFE_NO_PAD
            .decode(encoded)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .with_context(|| format!("key {text:?} is not 32 bytes of unpadded base64url"))?;
        let key = PublicKey(bytes);
        key.verifying_key()?;
        Ok(key)
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ed25519:{}", URL_SAFE_NO_PAD.encode(self.0))
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({self})")
    }
}

impl Serialize for PublicKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<PublicKey, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// The exact bytes a manifest's signature covers.
pub fn signed_bytes(manifest: &Value) -> Result<Vec<u8>> {
    bytes_under(SET_CONTEXT, manifest)
}

/// The key a manifest says signed it: `signed_by`, or `publisher` when it
/// has no `signed_by`.
pub fn signer(manifest: &Value) -> Result<PublicKey> {
    let key = manifest
        .get("signed_by")
        .or_else(|| manifest.get("publisher"))
        .and_then(Value::as_str)
        .context("manifest names no publisher")?;
    key.parse()
}

/// Signs `manifest` with `key`, setting its `signature`. `key` must be the
/// manifest's [`signer`].
pub fn sign(manifest: &mut Value, key: &SigningKey) -> Result<()> {
    let named = signer(manifest)?;
    sign_under(SET_CONTEXT, manifest, &named, key)
}

/// Checks `manifest`'s signature, and returns the key that made it.
pub fn verify(manifest: &Value) -> Result<PublicKey> {
    let key = signer(manifest)?;
    verify_under(SET_CONTEXT, manifest, &key).context("the manifest's signature")?;
    Ok(key)
}

/// `context` followed by the canonical JSON of `value` without its
/// `signature`.
fn bytes_under(context: &[u8], value: &Value) -> Result<Vec<u8>> {
    let mut unsigned = value
        .as_object()
        .context("a signed document is a JSON object")?
        .clone();
    unsigned.remove("signature");
    let mut bytes = context.to_vec();
    bytes.extend_from_slice(canonical_json(&Value::Object(unsigned))?.as_bytes());
    Ok(bytes)
}

/// Signs `value` under `context` with `key`, which must be `named`.
pub(crate) fn sign_under(
    context: &[u8],
    value: &mut Value,
    named: &PublicKey,
    key: &SigningKey,
) -> Result<()> {
    ensure!(
        *named == PublicKey::of(key),
        "the document names another key as its signer"
    );
    let signature = key.sign(&bytes_under(context, value)?);
    let encoded = URL_SAFE_NO_PAD.encode(signature.to_bytes());
    value
        .as_object_mut()
        .context("a signed document is a JSON object")?
        .insert("signature".to_string(), Value::String(encoded));
    Ok(())
}

/// Checks that `key` signed `value` under `context`.
pub(crate) fn verify_under(context: &[u8], value: &Value, key: &PublicKey) -> Result<()> {
    let Some(encoded) = value.get("signature").and_then(Value::as_str) else {
        bail!("it is not signed");
    };
    let bytes: [u8; 64] = URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .context("the signature is not 64 bytes of unpadded base64url")?;
    key.verifying_key()?
        .verify_strict(
            &bytes_under(context, value)?,
            &Signature::from_bytes(&bytes),
        )
        .context("the signature does not match")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn manifest(publisher: &SigningKey) -> Value {
        json!({
            "format": "plumb-set/1",
            "set": "plumb/test",
            "publisher": PublicKey::of(publisher).to_string(),
            "generation": "2026-10-12T00:00:00Z",
            "files": [{"path": "items-001.jsonl.zst", "count": 2, "bytes": 10, "sha256": "00"}]
        })
    }

    #[test]
    fn signs_and_checks() {
        let publisher = key(7);
        let mut signed = manifest(&publisher);
        sign(&mut signed, &publisher).unwrap();
        assert_eq!(verify(&signed).unwrap(), PublicKey::of(&publisher));
    }

    #[test]
    fn key_order_and_spacing_do_not_matter() {
        let publisher = key(7);
        let mut signed = manifest(&publisher);
        sign(&mut signed, &publisher).unwrap();
        let text = serde_json::to_string_pretty(&signed).unwrap();
        let reread: Value = serde_json::from_str(&text).unwrap();
        verify(&reread).unwrap();
    }

    #[test]
    fn any_change_breaks_it() {
        let publisher = key(7);
        let mut signed = manifest(&publisher);
        sign(&mut signed, &publisher).unwrap();
        let mut changed = signed.clone();
        changed["set"] = json!("plumb/other");
        assert!(verify(&changed).is_err());
        let mut added = signed.clone();
        added["labels"] = json!(["adult"]);
        assert!(verify(&added).is_err());
        let mut unsigned = signed;
        unsigned.as_object_mut().unwrap().remove("signature");
        assert!(verify(&unsigned).is_err());
    }

    #[test]
    fn only_the_named_key_signs() {
        let publisher = key(7);
        let stranger = key(8);
        let mut forged = manifest(&publisher);
        assert!(sign(&mut forged, &stranger).is_err());
        // A signature by another key, pasted in, does not check.
        let mut theirs = manifest(&stranger);
        sign(&mut theirs, &stranger).unwrap();
        forged["signature"] = theirs["signature"].clone();
        assert!(verify(&forged).is_err());
    }

    #[test]
    fn keys_round_trip() {
        let text = PublicKey::of(&key(7)).to_string();
        assert!(text.starts_with("ed25519:"));
        assert_eq!(text.parse::<PublicKey>().unwrap().to_string(), text);
        for bad in ["ed25519:", "ed25519:abc", "rsa:AAAA", &text[8..]] {
            assert!(bad.parse::<PublicKey>().is_err(), "{bad}");
        }
    }
}
