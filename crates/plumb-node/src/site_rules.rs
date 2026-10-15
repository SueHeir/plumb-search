//! Site rules sets ([`plumb_format::sites`]) in a node: reading a signed
//! set and putting it in place of the built-in rules
//! ([`plumb_core::built_in_rules`]).

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{ensure, Result};
use plumb_format::{Note, PublicKey, SiteRules};

use crate::sets::{check_held, read_items};

/// A site rules set read from its folder ([`read_site_rules`]).
#[derive(Debug, Clone)]
pub struct ReadRules {
    pub rules: SiteRules,
    /// Rules newer than this version understands, which it passes over.
    pub newer: usize,
    /// The SHA-256 of each of the set's parts, joined with `+`: it names
    /// the rules, so a set signed again with the same rules keeps it.
    pub hash: String,
}

/// Reads the site rules set in `dir`, after checking it as a node does a
/// set it holds ([`check_held`]): published by `trusted` or the key it
/// handed over to in `notes`.
pub fn read_site_rules(dir: &Path, trusted: &PublicKey, notes: &[Note]) -> Result<ReadRules> {
    check_held(dir, Some(trusted), notes)?;
    let (manifest, items) = read_items(dir)?;
    ensure!(
        manifest.is_site_rules(),
        "{} is not a site rules set",
        manifest.set
    );
    let mut rules = SiteRules::new();
    let newer = rules.add_items(&items).1.len();
    let hashes: Vec<&str> = manifest
        .files
        .iter()
        .map(|part| part.sha256.as_str())
        .collect();
    Ok(ReadRules {
        rules,
        newer,
        hash: hashes.join("+"),
    })
}

/// The [`ReadRules::hash`] of the site rules in use ([`use_site_rules`]);
/// `None` while the built-in lists are.
static IN_USE: OnceLock<String> = OnceLock::new();

/// Makes `read` say which site every host is, in place of the built-in
/// rules ([`plumb_core::use_site_rules`]). Once only, at start: false when
/// other rules already are in use.
pub fn use_site_rules(read: ReadRules) -> bool {
    let ReadRules { rules, hash, .. } = read;
    let used = plumb_core::use_site_rules(rules);
    if used {
        let _ = IN_USE.set(hash);
    }
    used
}

/// The [`ReadRules::hash`] of the site rules in use in place of the
/// built-in lists, or `None` while those are: what a node's records were
/// last made with is kept, so a node folds its seed data in again when
/// the rules change.
pub fn in_use() -> Option<&'static str> {
    IN_USE.get().map(String::as_str)
}
