//! The manifest: one small signed JSON file per set generation.
//!
//! It lets any node decide whether to take a set (who publishes it, its
//! license, its size), check it (the signature and a hash for each part),
//! index it without code for its kind (each field's type and roles), show
//! it (display templates), and keep it current (which generation it is
//! and what it replaces). The core reads every key but `hints`, which only
//! kind handlers read.
//!
//! A set is named `<scope>/<name>` (`plumb/wikipedia-en`, `liz/hn-stories`)
//! and identified together with its publisher. v0.2.0 nodes never ask for
//! a set whose name has a `/`, so they never receive one of these.

use std::collections::BTreeMap;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::item::Item;
use crate::notes::{current_publisher, is_revoked, Note};
use crate::refs::Ref;
use crate::sign::{verify, PublicKey};
use crate::value::{is_timestamp, FieldType};
use crate::FORMAT;

/// What this version of the format understands in a manifest's
/// `requires`. A set that requires anything else is skipped rather than
/// misread.
pub const UNDERSTOOD: &[&str] = &[];

/// The ref schemes a layer may extend.
const EXTENDABLE: &[&str] = &["wd", "site", "url", "id", "item", "blob"];

/// A set generation's manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Always [`FORMAT`]. Only ever gains optional keys.
    pub format: String,
    /// `<scope>/<name>`.
    pub set: String,
    pub publisher: PublicKey,
    /// The key that signed this generation, when not the publisher's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_by: Option<PublicKey>,
    /// The publisher's signed note that lets `signed_by` sign this set
    /// ([`crate::notes`]), as it was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<Value>,
    /// When this generation was built, a UTC timestamp. A node takes a
    /// generation only when it is later than the one it has.
    pub generation: String,
    /// The generation this one replaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
    /// How often to look for a newer generation (`7d`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// Where takedown notices go.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact: Option<String>,
    /// Where the data comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// An SPDX expression. A set without one is never passed on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Shown with results from the set (`From Wikipedia ({url})`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
    /// Which kind handlers to offer the set to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    /// For a layer: the ref schemes its entries extend (`wd`, `site`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extends: Vec<String>,
    /// Labels for every item of the set (`adult`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Features a node must understand to read the set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// How items are ordered (`weight`: most weighted first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<String>,
    /// What `weight` counts and how it scales.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<Value>,
    /// The set's parts, in order, each with its own hash.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FilePart>,
    /// Changes and tombstones since an earlier generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<Value>,
    /// Every field the set's items use, declared once.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, FieldSpec>,
    /// Display templates: plain text with `{field}` slots.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub show: BTreeMap<String, String>,
    /// Generic placement among results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<Value>,
    /// The set's own caps per item, within the format's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Value>,
    /// For kind handlers only; the core never reads it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hints: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Keys this version doesn't know, kept as they came.
    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

/// One part of a set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FilePart {
    /// A file name beside the manifest.
    pub path: String,
    /// Items in it.
    pub count: u64,
    /// Its size as stored.
    pub bytes: u64,
    /// The hex SHA-256 of the file as stored, after compression.
    pub sha256: String,
    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

/// How a set declares one of its fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldSpec {
    #[serde(rename = "type")]
    pub field_type: FieldType,
    /// True when the field holds a list of values.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub list: bool,
    /// For numbers: a UCUM unit (`m`, `min`) or ISO 4217 currency (`USD`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// The Wikidata property the field means (`wd:P36`). Fields of two sets
    /// are the same thing only when both name the same property.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_as: Option<Ref>,
    /// `search:name`, `search:about`, `search:body`, `filter`, `show`,
    /// `answer`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

impl Manifest {
    /// Reads a manifest from JSON, checking its signature and its rules.
    /// Returns it with the key that signed it. A manifest signed by another
    /// key than its publisher's must carry the publisher's delegation to
    /// that key, covering its set at `now`, a UTC timestamp.
    ///
    /// Whether to take it is a separate question: see [`Manifest::trusted`].
    pub fn from_signed_json(text: &str, now: &str) -> Result<(Manifest, PublicKey)> {
        ensure!(is_timestamp(now), "now {now:?} is not a UTC timestamp");
        let value: Value = serde_json::from_str(text).context("the manifest is not JSON")?;
        let key = verify(&value)?;
        let manifest: Manifest = serde_json::from_value(value)?;
        manifest.check()?;
        if key != manifest.publisher {
            let delegation = manifest.delegation.as_ref().with_context(|| {
                format!(
                    "{} is signed by {key}, not its publisher, and carries no delegation",
                    manifest.set
                )
            })?;
            let note = Note::from_signed_value(delegation).context("the delegation")?;
            ensure!(
                note.kind == "delegate" && note.by == manifest.publisher && note.key == key,
                "the delegation is not from {} to {key}",
                manifest.publisher
            );
            ensure!(
                note.delegates(&manifest.set, now),
                "the delegation does not cover {} at {now}",
                manifest.set
            );
        }
        Ok((manifest, key))
    }

    /// Checks that a node that trusts `trusted` and holds the handovers and
    /// revocations in `notes` takes this manifest, signed by `signer`: its
    /// publisher must be the key `trusted` hands over to, and `signer` must
    /// not be revoked.
    pub fn trusted(&self, signer: &PublicKey, trusted: &PublicKey, notes: &[Note]) -> Result<()> {
        let publisher = current_publisher(trusted, notes).with_context(|| {
            format!("a key handed over from {trusted} was handed over to two keys")
        })?;
        ensure!(
            self.publisher == publisher,
            "{} is published by {}, not {publisher}",
            self.set,
            self.publisher
        );
        ensure!(
            !is_revoked(signer, &self.publisher, notes),
            "{signer}, which signed {}, is revoked",
            self.set
        );
        Ok(())
    }

    /// Checks the rules every manifest keeps.
    pub fn check(&self) -> Result<()> {
        ensure!(
            self.format == FORMAT,
            "format {:?} is not {FORMAT}",
            self.format
        );
        ensure!(
            is_set_name(&self.set),
            "{:?} is not a set name, <scope>/<name>",
            self.set
        );
        ensure!(
            is_timestamp(&self.generation),
            "generation {:?} is not a UTC timestamp",
            self.generation
        );
        if let Some(prev) = &self.prev {
            ensure!(
                is_timestamp(prev) && prev < &self.generation,
                "prev {prev:?} is not a timestamp before this generation"
            );
        }
        ensure!(
            !self.files.is_empty() || self.delta.is_some(),
            "{} has no files and no delta",
            self.set
        );
        for part in &self.files {
            ensure!(
                !part.path.is_empty()
                    && !part.path.contains(['/', '\\'])
                    && !part.path.starts_with('.'),
                "{:?} is not a file name beside the manifest",
                part.path
            );
            ensure!(
                part.sha256.len() == 64
                    && part
                        .sha256
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{}: sha256 is not 64 lower-case hex digits",
                part.path
            );
        }
        for scheme in &self.extends {
            ensure!(
                EXTENDABLE.contains(&scheme.as_str()),
                "a layer cannot extend {scheme:?} refs"
            );
        }
        for (name, spec) in &self.fields {
            if let Some(same_as) = &spec.same_as {
                ensure!(
                    matches!(same_as, Ref::Wd(id) if id.starts_with('P')),
                    "field {name:?}: same_as {same_as} is not a Wikidata property"
                );
            }
        }
        Ok(())
    }

    /// The entries of `requires` this version does not understand. A node
    /// skips a set with any, and says a newer version is needed.
    pub fn not_understood(&self) -> Vec<&str> {
        self.requires
            .iter()
            .map(String::as_str)
            .filter(|feature| !UNDERSTOOD.contains(feature))
            .collect()
    }

    /// Checks an item of this set: the rules every item keeps, an
    /// extension's target against the schemes the set extends, and each
    /// field against its declared type.
    pub fn check_item(&self, item: &Item) -> Result<()> {
        item.check()?;
        if let Some(target) = &item.extends {
            let scheme = target.to_string();
            let scheme = scheme.split(':').next().unwrap_or_default();
            ensure!(
                self.extends.iter().any(|s| s == scheme),
                "item {:?} extends a {scheme}: ref, which {} does not declare",
                item.id,
                self.set
            );
        }
        for (name, value) in &item.fields {
            let Some(spec) = self.fields.get(name) else {
                bail!(
                    "item {:?}: field {name:?} is not declared by {}",
                    item.id,
                    self.set
                );
            };
            spec.field_type
                .check_field(spec.list, value)
                .with_context(|| format!("item {:?}: field {name:?}", item.id))?;
        }
        Ok(())
    }
}

/// True for `<scope>/<name>`: two parts of lower-case ASCII letters,
/// digits and hyphens, each starting with a letter or digit.
pub fn is_set_name(name: &str) -> bool {
    let part = |p: &str| {
        (1..=64).contains(&p.len())
            && !p.starts_with('-')
            && p.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    matches!(name.split_once('/'), Some((scope, rest)) if part(scope) && part(rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sign::sign;
    use ed25519_dalek::SigningKey;
    use serde_json::json;

    const HASH: &str = "9f2c3b0e7d6a5f4e3d2c1b0a9f8e7d6c5b4a39281706f5e4d3c2b1a098f7e6d5";
    const NOW: &str = "2026-10-12T06:00:00Z";

    fn facts_layer(key: &SigningKey) -> Value {
        json!({
            "format": "plumb-set/1",
            "set": "plumb/wikidata-facts",
            "publisher": PublicKey::of(key).to_string(),
            "generation": "2026-10-11T05:00:00Z",
            "prev": "2026-10-04T05:00:00Z",
            "every": "7d",
            "license": "CC0-1.0",
            "attribution": "From Wikidata",
            "extends": ["wd"],
            "files": [{"path": "items-001.jsonl.zst", "count": 100000, "bytes": 2048, "sha256": HASH}],
            "fields": {
                "capital": {"type": "ref", "same_as": "wd:P36", "roles": ["show", "answer"]},
                "population": {"type": "number", "same_as": "wd:P1082", "roles": ["show", "answer", "filter"]}
            },
            "hints": {"facts": {"best_rank_only": true}},
            "endorsement": "kept, though this version does not read it"
        })
    }

    fn signed(value: &mut Value, key: &SigningKey) -> String {
        sign(value, key).unwrap();
        serde_json::to_string(value).unwrap()
    }

    #[test]
    fn reads_a_signed_layer() {
        let key = SigningKey::from_bytes(&[3; 32]);
        let text = signed(&mut facts_layer(&key), &key);
        let (manifest, signer) = Manifest::from_signed_json(&text, NOW).unwrap();
        assert_eq!(signer, PublicKey::of(&key));
        assert_eq!(manifest.set, "plumb/wikidata-facts");
        assert_eq!(manifest.fields["capital"].field_type, FieldType::Ref);
        assert!(manifest.other.contains_key("endorsement"));
        assert!(manifest.not_understood().is_empty());

        let entry: Item = serde_json::from_value(json!({
            "id": "Q408", "extends": "wd:Q408",
            "fields": {"capital": "wd:Q3114", "population": {"v": 26000000, "as_of": "2021"}}
        }))
        .unwrap();
        manifest.check_item(&entry).unwrap();

        let undeclared: Item = serde_json::from_value(json!({
            "id": "Q408", "extends": "wd:Q408", "fields": {"anthem": "Advance Australia Fair"}
        }))
        .unwrap();
        assert!(manifest.check_item(&undeclared).is_err());
        let wrong_type: Item = serde_json::from_value(json!({
            "id": "Q408", "extends": "wd:Q408", "fields": {"population": "lots"}
        }))
        .unwrap();
        assert!(manifest.check_item(&wrong_type).is_err());
        let wrong_target: Item = serde_json::from_value(json!({
            "id": "x", "extends": "site:python.org", "fields": {"population": 1}
        }))
        .unwrap();
        assert!(manifest.check_item(&wrong_target).is_err());
    }

    #[test]
    fn unsigned_or_broken_manifests_are_refused() {
        let key = SigningKey::from_bytes(&[3; 32]);
        let unsigned = serde_json::to_string(&facts_layer(&key)).unwrap();
        assert!(Manifest::from_signed_json(&unsigned, NOW).is_err());

        let breakages: [(&str, Value); 6] = [
            ("format", json!("plumb-set/2")),
            ("set", json!("wikipedia-en")),
            ("generation", json!("2026-10-11")),
            ("prev", json!("2026-10-12T00:00:00Z")),
            ("extends", json!(["magnet"])),
            (
                "files",
                json!([{"path": "../x", "count": 1, "bytes": 1, "sha256": HASH}]),
            ),
        ];
        for (key_name, bad) in breakages {
            let mut value = facts_layer(&key);
            value[key_name] = bad;
            let text = signed(&mut value, &key);
            assert!(
                Manifest::from_signed_json(&text, NOW).is_err(),
                "{key_name}"
            );
        }
    }

    #[test]
    fn delegated_signing() {
        use crate::notes::tests::note;
        let (root, builder) = (
            SigningKey::from_bytes(&[3; 32]),
            SigningKey::from_bytes(&[4; 32]),
        );
        let (root_key, builder_key) = (PublicKey::of(&root), PublicKey::of(&builder));
        let until = "2027-01-10T00:00:00Z";
        let by_builder = |delegation: Option<Value>| {
            let mut value = facts_layer(&root);
            value["signed_by"] = json!(builder_key.to_string());
            if let Some(delegation) = delegation {
                value["delegation"] = delegation;
            }
            signed(&mut value, &builder)
        };
        let delegation = note(
            "delegate",
            3,
            4,
            json!({"sets": ["plumb/wikidata-*"], "until": until}),
        );

        let text = by_builder(Some(delegation.clone()));
        let (manifest, signer) = Manifest::from_signed_json(&text, NOW).unwrap();
        assert_eq!((manifest.publisher, signer), (root_key, builder_key));
        manifest.trusted(&signer, &root_key, &[]).unwrap();
        assert!(Manifest::from_signed_json(&text, "2027-01-10T00:00:01Z").is_err());

        let other_sets = note(
            "delegate",
            3,
            4,
            json!({"sets": ["plumb/docs"], "until": until}),
        );
        let from_stranger = note(
            "delegate",
            5,
            4,
            json!({"sets": ["plumb/wikidata-*"], "until": until}),
        );
        let not_a_delegation = note("handover", 3, 4, json!({}));
        for refused in [
            None,
            Some(other_sets),
            Some(from_stranger),
            Some(not_a_delegation),
        ] {
            assert!(Manifest::from_signed_json(&by_builder(refused), NOW).is_err());
        }
        let mut edited = delegation;
        edited["sets"] = json!(["plumb/*"]);
        assert!(Manifest::from_signed_json(&by_builder(Some(edited)), NOW).is_err());

        let revoked = Note::from_signed_value(&note("revoke", 3, 4, json!({}))).unwrap();
        assert!(manifest.trusted(&signer, &root_key, &[revoked]).is_err());
        assert!(manifest.trusted(&signer, &builder_key, &[]).is_err());
    }

    #[test]
    fn a_handover_moves_trust_to_the_new_key() {
        use crate::notes::tests::note;
        let (old, new) = (
            SigningKey::from_bytes(&[3; 32]),
            SigningKey::from_bytes(&[4; 32]),
        );
        let old_key = PublicKey::of(&old);
        let handover = Note::from_signed_value(&note("handover", 3, 4, json!({}))).unwrap();
        let (by_new, signer) =
            Manifest::from_signed_json(&signed(&mut facts_layer(&new), &new), NOW).unwrap();
        assert!(by_new.trusted(&signer, &old_key, &[]).is_err());
        by_new
            .trusted(&signer, &old_key, std::slice::from_ref(&handover))
            .unwrap();
        let (by_old, signer) =
            Manifest::from_signed_json(&signed(&mut facts_layer(&old), &old), NOW).unwrap();
        by_old.trusted(&signer, &old_key, &[]).unwrap();
        assert!(by_old.trusted(&signer, &old_key, &[handover]).is_err());
    }

    #[test]
    fn requires_what_it_cannot_read() {
        let key = SigningKey::from_bytes(&[3; 32]);
        let mut value = facts_layer(&key);
        value["requires"] = json!(["places-rows/1"]);
        let (manifest, _) = Manifest::from_signed_json(&signed(&mut value, &key), NOW).unwrap();
        assert_eq!(manifest.not_understood(), ["places-rows/1"]);
    }

    #[test]
    fn set_names() {
        for good in ["plumb/wikipedia-en", "liz/hn-stories", "a/b"] {
            assert!(is_set_name(good), "{good}");
        }
        for bad in [
            "wikipedia-en",
            "plumb/wiki/en",
            "Plumb/x",
            "plumb/",
            "/x",
            "plumb/-x",
        ] {
            assert!(!is_set_name(bad), "{bad}");
        }
    }
}
