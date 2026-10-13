//! Site rules sets ([`plumb_format::sites`]) in a node: the built-in site
//! lists as rules, and reading a signed set and putting it in their place.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{ensure, Result};
use plumb_core::subsites::{NOT_SITES, SUBDOMAIN_SITES, UMBRELLA_DOMAINS};
use plumb_core::SiteList;
use plumb_format::sites::Rule;
use plumb_format::{Note, PublicKey, SiteRules};

use crate::sets::{check_held, read_items};

/// The site lists built into `plumb-core` as rules: subsites, umbrella
/// domains and the labels that are never sites under one. The suffix list
/// itself comes from a Public Suffix List file.
pub fn built_in_rules() -> Vec<Rule> {
    let subsites = SUBDOMAIN_SITES
        .iter()
        .map(|site| Rule::Subsite(site.to_string()));
    let umbrellas = UMBRELLA_DOMAINS
        .iter()
        .map(|domain| Rule::Umbrella(domain.to_string()));
    let not_sites = NOT_SITES
        .iter()
        .map(|label| Rule::NotSite(label.to_string()));
    subsites.chain(umbrellas).chain(not_sites).collect()
}

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
/// lists ([`plumb_core::use_site_list`]). Once only, at start: false when
/// other rules already are in use.
pub fn use_site_rules(read: ReadRules) -> bool {
    let ReadRules { rules, hash, .. } = read;
    let used = plumb_core::use_site_list(Box::new(Rules(rules)));
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

struct Rules(SiteRules);

impl SiteList for Rules {
    fn suffix<'h>(&self, host: &'h str) -> &'h str {
        self.0.suffix(host)
    }

    fn site_of(&self, host: &str) -> Option<String> {
        self.0.site_of(host)
    }

    fn subsites(&self) -> Vec<&str> {
        self.0.subsites().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_lists_read_back_as_rules() {
        for rule in built_in_rules() {
            assert_eq!(Rule::parse(&rule.to_string()).unwrap(), Some(rule));
        }
    }
}
