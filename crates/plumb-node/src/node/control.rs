//! Remote control: letting another computer, such as the desktop app on a
//! laptop, change this node's settings.
//!
//! Off until the node's owner turns it on, on the node's own panel (from
//! the computer it runs on) or with `plumb remote-control on`, which makes a
//! new token and shows it once. The node keeps only the token's SHA-256 in
//! `DIR/remote-control.json`; turning remote control off deletes the file.
//! Requests carry the token as `Authorization: Bearer <token>` and may only
//! read the node's status and change its settings, features and refreshes
//! (see [`crate::web`]'s `/api/control`).
//!
//! Since the file is read for every request, turning remote control on or
//! off, or making a new token, takes effect at once, without a restart.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::store;

/// The file that turns remote control on.
pub const FILE_NAME: &str = "remote-control.json";

/// Every token starts with this, so it is easy to recognize when pasted.
pub const TOKEN_PREFIX: &str = "plumb_";

/// Remote control as turned on: the token's hash and where it may be used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteControl {
    /// Hex SHA-256 of the token.
    token_sha256: String,
    /// Take requests from public addresses and through reverse proxies too.
    /// Off: only from this computer, local networks (private, link-local
    /// and unique local addresses) and CGNAT/Tailscale addresses.
    #[serde(default)]
    pub allow_public: bool,
    /// When the token was made, in Unix seconds.
    #[serde(default)]
    pub created: u64,
}

impl RemoteControl {
    /// Whether `token` is the one remote control was turned on with. The
    /// hashes are compared in constant time.
    pub fn accepts(&self, token: &str) -> bool {
        let Ok(expected) = hex_decode(&self.token_sha256) else {
            return false;
        };
        let given = Sha256::digest(token.trim().as_bytes());
        given.as_slice().ct_eq(&expected).into()
    }
}

fn path(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

/// Remote control as turned on in data folder `dir`; `None` when it is off.
pub fn load(dir: &Path) -> Result<Option<RemoteControl>> {
    match std::fs::read(path(dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("reading {FILE_NAME}")),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {FILE_NAME}")),
    }
}

/// Turns remote control on with a new token, which replaces any earlier
/// one, and returns the token. It is not kept anywhere: this is the only
/// time it can be read.
pub fn turn_on(dir: &Path, allow_public: bool) -> Result<String> {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(|err| anyhow::anyhow!("no random numbers: {err}"))?;
    let token = format!("{TOKEN_PREFIX}{}", hex_encode(&secret));
    let control = RemoteControl {
        token_sha256: hex_encode(&Sha256::digest(token.as_bytes())),
        allow_public,
        created: plumb_core::now_unix(),
    };
    store::write_atomically(&path(dir), &serde_json::to_vec_pretty(&control)?)?;
    Ok(token)
}

/// Turns remote control off; `false` when it already was.
pub fn turn_off(dir: &Path) -> Result<bool> {
    match std::fs::remove_file(path(dir)) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("deleting {FILE_NAME}")),
    }
}

/// Whether a peer at `ip` is close enough to send control requests without
/// [`RemoteControl::allow_public`]: this computer, a local network, or a
/// CGNAT range (which Tailscale uses).
pub fn is_nearby(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip.to_canonical() {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            ip.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn hex_decode(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        bail!("not hex");
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).context("not hex"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_off_until_turned_on_and_accepts_only_its_token() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), None);

        let token = turn_on(dir.path(), false).unwrap();
        assert!(token.starts_with(TOKEN_PREFIX));
        assert_eq!(token.len(), TOKEN_PREFIX.len() + 64);
        let saved = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(!saved.contains(&token), "only the hash is kept: {saved}");

        let control = load(dir.path()).unwrap().unwrap();
        assert!(control.accepts(&token));
        assert!(control.accepts(&format!(" {token}\n")));
        assert!(!control.accepts(""));
        assert!(!control.accepts(&token[..token.len() - 1]));
        assert!(!control.accepts(&format!("{token}0")));

        // A new token replaces the old one.
        let newer = turn_on(dir.path(), true).unwrap();
        let control = load(dir.path()).unwrap().unwrap();
        assert!(control.accepts(&newer));
        assert!(!control.accepts(&token));
        assert!(control.allow_public);

        assert!(turn_off(dir.path()).unwrap());
        assert!(!turn_off(dir.path()).unwrap());
        assert_eq!(load(dir.path()).unwrap(), None);
    }

    #[test]
    fn nearby_means_this_computer_or_a_local_network() {
        for ip in [
            "127.0.0.1",
            "10.0.0.5",
            "172.17.0.1",
            "192.168.1.20",
            "169.254.3.4",
            "100.101.102.103",
            "::1",
            "::ffff:192.168.1.20",
            "fd12:3456::1",
            "fe80::1",
        ] {
            assert!(is_nearby(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "198.211.114.63",
            "8.8.8.8",
            "100.128.0.1",
            "2001:db8::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_nearby(ip.parse().unwrap()), "{ip}");
        }
    }
}
