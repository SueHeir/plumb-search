//! Key notes: small signed statements that let one key stand for another.
//!
//! | Kind | Says | Signed by |
//! |---|---|---|
//! | `delegate` | `key` may sign the sets in `sets` until `until`: "hpc may sign `plumb/wikipedia-*` until 2027-01-10" | the publisher |
//! | `handover` | `key` publishes from now on in place of `by`, as when the project moves from its hpc key to an offline one | the old key |
//! | `revoke` | manifests signed by `key` are no longer taken | `key` itself, or the publisher that delegated to it |
//!
//! A manifest signed by a delegated key carries its delegation note
//! (`Manifest::delegation`). Handovers and revocations travel on their
//! own, like denylist entries, and a node keeps those it has seen.
//!
//! A note is signed by its `by` key over `plumb-note/1\0` followed by its
//! canonical JSON without its `signature` key ([`crate::sign`]).
//!
//! Following handovers trusts the old key once more: whoever holds it can
//! name a new one. So a key that hands over to two different keys hands
//! over to neither, and a node that sees that takes nothing from that line
//! of keys until its owner ships a new trusted key. Only a stolen key would
//! sign both.

use std::collections::BTreeMap;

use anyhow::{bail, ensure, Context, Result};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::is_set_name;
use crate::sign::{sign_under, verify_under, PublicKey, NOTE_CONTEXT};
use crate::value::is_timestamp;

/// The `format` of every note.
pub const NOTE_FORMAT: &str = "plumb-note/1";

/// The note kinds this version reads. A note of another kind is refused,
/// never half read.
pub const KINDS: &[&str] = &["delegate", "handover", "revoke"];

/// A key note.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    /// Always [`NOTE_FORMAT`].
    pub format: String,
    /// One of [`KINDS`].
    pub kind: String,
    /// The key that signs the note.
    pub by: PublicKey,
    /// The key the note is about.
    pub key: PublicKey,
    /// For a delegation: the set names it covers, each a set name or
    /// `<scope>/<start>*` ([`is_set_pattern`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sets: Vec<String>,
    /// When it was signed, a UTC timestamp.
    pub issued: String,
    /// For a delegation: when it ends, a UTC timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

impl Note {
    /// Reads a note, checking its signature and its rules.
    pub fn from_signed_value(value: &Value) -> Result<Note> {
        let note: Note = serde_json::from_value(value.clone()).context("not a key note")?;
        verify_under(NOTE_CONTEXT, value, &note.by).context("the note's signature")?;
        note.check()?;
        Ok(note)
    }

    /// Checks the rules every note keeps.
    pub fn check(&self) -> Result<()> {
        ensure!(
            self.format == NOTE_FORMAT,
            "format {:?} is not {NOTE_FORMAT}",
            self.format
        );
        ensure!(
            is_timestamp(&self.issued),
            "issued {:?} is not a UTC timestamp",
            self.issued
        );
        match self.kind.as_str() {
            "delegate" => {
                ensure!(self.by != self.key, "a key cannot delegate to itself");
                ensure!(!self.sets.is_empty(), "the delegation names no sets");
                for pattern in &self.sets {
                    ensure!(
                        is_set_pattern(pattern),
                        "{pattern:?} is not a set name or <scope>/<start>*"
                    );
                }
                let until = self.until.as_deref().context("the delegation has no end")?;
                ensure!(
                    is_timestamp(until) && until > self.issued.as_str(),
                    "until {until:?} is not a UTC timestamp after it was issued"
                );
            }
            "handover" | "revoke" => {
                ensure!(
                    self.kind == "revoke" || self.by != self.key,
                    "a key cannot hand over to itself"
                );
                ensure!(
                    self.sets.is_empty() && self.until.is_none(),
                    "a {} note has no sets and no end",
                    self.kind
                );
            }
            kind => bail!("unknown note kind {kind:?}"),
        }
        Ok(())
    }

    /// True when this is a delegation that lets its key sign `set` at
    /// `now`, a UTC timestamp.
    pub fn delegates(&self, set: &str, now: &str) -> bool {
        self.kind == "delegate"
            && self.until.as_deref().is_some_and(|until| now <= until)
            && self.sets.iter().any(|pattern| matches(pattern, set))
    }
}

/// Signs `note` with `key`, setting its `signature`. `key` must be the
/// note's `by` key.
pub fn sign_note(note: &mut Value, key: &SigningKey) -> Result<()> {
    let by: PublicKey = note
        .get("by")
        .and_then(Value::as_str)
        .context("the note names no by key")?
        .parse()?;
    sign_under(NOTE_CONTEXT, note, &by, key)
}

/// True for a set name, or `<scope>/<start>*`: every set of that scope
/// whose name starts with `<start>`, which may be empty.
pub fn is_set_pattern(pattern: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => prefix.split_once('/').is_some_and(|(scope, start)| {
            is_set_name(&format!(
                "{scope}/{}",
                if start.is_empty() { "x" } else { start }
            ))
        }),
        None => is_set_name(pattern),
    }
}

fn matches(pattern: &str, set: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => set.starts_with(prefix),
        None => pattern == set,
    }
}

/// The key a node that trusts `trusted` takes new generations from, given
/// the handovers in `notes`: `trusted`, or the last key of the handovers
/// that start from it. `None` when a key on the way handed over to two
/// different keys.
pub fn current_publisher(trusted: &PublicKey, notes: &[Note]) -> Option<PublicKey> {
    let mut key = *trusted;
    let mut seen = vec![key];
    loop {
        let mut next = notes
            .iter()
            .filter(|note| note.kind == "handover" && note.by == key)
            .map(|note| note.key);
        let Some(first) = next.next() else {
            return Some(key);
        };
        if next.any(|other| other != first) {
            return None;
        }
        if seen.contains(&first) {
            // Handed back to an earlier key: that key publishes.
            return Some(first);
        }
        seen.push(first);
        key = first;
    }
}

/// True when `notes` revoke `signer` for sets published by `publisher`: a
/// revocation signed by `signer` itself, or by `publisher`.
pub fn is_revoked(signer: &PublicKey, publisher: &PublicKey, notes: &[Note]) -> bool {
    notes.iter().any(|note| {
        note.kind == "revoke"
            && note.key == *signer
            && (note.by == *signer || note.by == *publisher)
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn public(seed: u8) -> PublicKey {
        PublicKey::of(&key(seed))
    }

    pub(crate) fn note(kind: &str, by: u8, about: u8, extra: Value) -> Value {
        let mut value = json!({
            "format": NOTE_FORMAT,
            "kind": kind,
            "by": public(by).to_string(),
            "key": public(about).to_string(),
            "issued": "2026-10-12T00:00:00Z",
        });
        for (name, member) in extra.as_object().unwrap() {
            value[name] = member.clone();
        }
        sign_note(&mut value, &key(by)).unwrap();
        value
    }

    fn read(value: &Value) -> Note {
        Note::from_signed_value(value).unwrap()
    }

    #[test]
    fn delegations() {
        let value = note(
            "delegate",
            1,
            2,
            json!({"sets": ["plumb/wikipedia-*", "plumb/docs"], "until": "2027-01-10T00:00:00Z"}),
        );
        let note = read(&value);
        assert!(note.delegates("plumb/wikipedia-en", "2026-11-01T00:00:00Z"));
        assert!(note.delegates("plumb/docs", "2027-01-10T00:00:00Z"));
        assert!(!note.delegates("plumb/docs2", "2026-11-01T00:00:00Z"));
        assert!(!note.delegates("liz/wikipedia-en", "2026-11-01T00:00:00Z"));
        assert!(!note.delegates("plumb/docs", "2027-01-10T00:00:01Z"));
    }

    #[test]
    fn a_note_is_checked_against_its_own_signer() {
        let value = note("handover", 1, 2, json!({}));
        let mut changed = value.clone();
        changed["key"] = json!(public(3).to_string());
        assert!(Note::from_signed_value(&changed).is_err());
        // Signed by someone else than its by key.
        let mut forged = value.clone();
        forged["by"] = json!(public(3).to_string());
        assert!(Note::from_signed_value(&forged).is_err());
        let mut unsigned = value;
        unsigned.as_object_mut().unwrap().remove("signature");
        assert!(Note::from_signed_value(&unsigned).is_err());
    }

    #[test]
    fn a_note_never_passes_for_a_manifest() {
        let value = note("handover", 1, 2, json!({}));
        verify_under(NOTE_CONTEXT, &value, &public(1)).unwrap();
        assert!(verify_under(crate::sign::SET_CONTEXT, &value, &public(1)).is_err());
    }

    #[test]
    fn broken_notes() {
        let until = json!({"until": "2027-01-10T00:00:00Z"});
        for (kind, by, about, extra) in [
            ("delegate", 1, 2, json!({"sets": ["plumb/docs"]})),
            ("delegate", 1, 2, until.clone()),
            (
                "delegate",
                1,
                1,
                json!({"sets": ["plumb/docs"], "until": "2027-01-10T00:00:00Z"}),
            ),
            (
                "delegate",
                1,
                2,
                json!({"sets": ["*"], "until": "2027-01-10T00:00:00Z"}),
            ),
            (
                "delegate",
                1,
                2,
                json!({"sets": ["plumb/docs"], "until": "2026-01-01T00:00:00Z"}),
            ),
            ("handover", 1, 1, json!({})),
            ("handover", 1, 2, until),
            ("trust", 1, 2, json!({})),
        ] {
            let value = note(kind, by, about, extra);
            assert!(Note::from_signed_value(&value).is_err(), "{value}");
        }
        read(&note("revoke", 1, 1, json!({})));
    }

    #[test]
    fn set_patterns() {
        for good in ["plumb/docs", "plumb/wikipedia-*", "plumb/*", "liz/notes"] {
            assert!(is_set_pattern(good), "{good}");
        }
        for bad in ["*", "plumb", "plumb/*/x", "Plumb/*", "/docs", "plumb/doc*s"] {
            assert!(!is_set_pattern(bad), "{bad}");
        }
    }

    #[test]
    fn handovers_are_followed_unless_they_disagree() {
        let (a, b, c) = (public(1), public(2), public(3));
        assert_eq!(current_publisher(&a, &[]), Some(a));
        let a_to_b = read(&note("handover", 1, 2, json!({})));
        let b_to_c = read(&note("handover", 2, 3, json!({})));
        assert_eq!(
            current_publisher(&a, std::slice::from_ref(&a_to_b)),
            Some(b)
        );
        assert_eq!(
            current_publisher(&a, &[b_to_c.clone(), a_to_b.clone()]),
            Some(c)
        );
        assert_eq!(
            current_publisher(&b, std::slice::from_ref(&a_to_b)),
            Some(b)
        );
        let a_to_c = read(&note("handover", 1, 3, json!({})));
        assert_eq!(current_publisher(&a, &[a_to_b.clone(), a_to_c]), None);
        let b_to_a = read(&note("handover", 2, 1, json!({})));
        assert_eq!(current_publisher(&a, &[a_to_b, b_to_a]), Some(a));
    }

    #[test]
    fn revocations() {
        let (publisher, builder, stranger) = (public(1), public(2), public(3));
        let by_itself = read(&note("revoke", 2, 2, json!({})));
        let by_publisher = read(&note("revoke", 1, 2, json!({})));
        let by_stranger = read(&note("revoke", 3, 2, json!({})));
        assert!(is_revoked(&builder, &publisher, &[by_itself]));
        assert!(is_revoked(&builder, &publisher, &[by_publisher]));
        assert!(!is_revoked(
            &builder,
            &publisher,
            std::slice::from_ref(&by_stranger)
        ));
        assert!(!is_revoked(&stranger, &publisher, &[by_stranger]));
    }
}
