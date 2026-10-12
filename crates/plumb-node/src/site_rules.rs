//! Site rules sets ([`plumb_format::sites`]) in a node: the built-in site
//! lists as rules, reading a signed set and putting it in their place, and
//! the hosts whose site a set of rules would move.

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{ensure, Context, Result};
use plumb_core::subsites::{NOT_SITES, SUBDOMAIN_SITES, UMBRELLA_DOMAINS};
use plumb_core::SiteList;
use plumb_format::sites::Rule;
use plumb_format::{Item, Manifest, Note, PublicKey, SiteRules};

use crate::sets::{check_set, now, MANIFEST};

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

/// Reads the site rules set in `dir`, after checking it as a node does
/// before taking a set ([`check_set`]): published by `trusted` or the key
/// it handed over to in `notes`.
pub fn read_site_rules(dir: &Path, trusted: &PublicKey, notes: &[Note]) -> Result<ReadRules> {
    check_set(dir, Some(trusted), notes)?;
    let text = fs::read_to_string(dir.join(MANIFEST))?;
    let (manifest, _) = Manifest::from_signed_json(&text, &now())?;
    ensure!(
        manifest.is_site_rules(),
        "{} is not a site rules set",
        manifest.set
    );
    let mut rules = SiteRules::new();
    let mut newer = 0;
    for part in &manifest.files {
        let path = dir.join(&part.path);
        let reader = BufReader::new(zstd::Decoder::new(File::open(&path)?)?);
        let mut items = Vec::new();
        for line in reader.lines() {
            let line = line.with_context(|| format!("reading {}", path.display()))?;
            items.push(serde_json::from_str::<Item>(&line)?);
        }
        newer += rules.add_items(&items).1.len();
    }
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

/// A host to check for each way `rules` can decide a site: each name a
/// rule names, the names just under it, and a `www.` and a deeper name
/// under those.
pub fn hosts_to_check(rules: &[Rule]) -> Vec<String> {
    let umbrellas: Vec<&str> = rules
        .iter()
        .filter_map(|rule| match rule {
            Rule::Umbrella(domain) => Some(domain.as_str()),
            _ => None,
        })
        .collect();
    let mut hosts = vec!["example.zzzunknown".to_string(), "zzzunknown".to_string()];
    for rule in rules {
        match rule {
            Rule::Suffix(name)
            | Rule::Exception(name)
            | Rule::Subsite(name)
            | Rule::Umbrella(name) => {
                for under in ["", "a.", "www.a.", "b.a.", "www.", "m."] {
                    hosts.push(format!("{under}{name}"));
                }
            }
            Rule::Wildcard(name) => {
                for under in ["", "x.", "a.x.", "www.a.x.", "b.a.x."] {
                    hosts.push(format!("{under}{name}"));
                }
            }
            Rule::NotSite(label) => {
                for domain in &umbrellas {
                    hosts.push(format!("{label}.{domain}"));
                    hosts.push(format!("a.{label}.{domain}"));
                }
            }
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// A host whose site differs between the built-in lists and a set of
/// rules.
#[derive(Debug, PartialEq, Eq)]
pub struct Moved {
    pub host: String,
    pub built_in: Option<String>,
    pub by_rules: Option<String>,
}

/// The hosts (or URLs) among `hosts` whose site by `rules` is not the one
/// the built-in lists give ([`plumb_core::built_in_site`]). Hosts that are
/// no website's are passed over.
pub fn moved_sites<'a>(rules: &SiteRules, hosts: impl IntoIterator<Item = &'a str>) -> Vec<Moved> {
    hosts
        .into_iter()
        .filter_map(|input| {
            let host = plumb_core::host_of(input)?;
            let built_in = plumb_core::built_in_site(&host);
            let by_rules = rules.site_of(&host);
            (built_in != by_rules).then_some(Moved {
                host,
                built_in,
                by_rules,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use plumb_format::sites::read_suffix_list;

    fn rules_of(text: &str) -> (Vec<Rule>, SiteRules) {
        let mut rules = read_suffix_list(text).unwrap();
        rules.extend(built_in_rules());
        let set = rules.iter().cloned().collect();
        (rules, set)
    }

    #[test]
    fn the_built_in_lists_read_back_as_rules() {
        for rule in built_in_rules() {
            assert_eq!(Rule::parse(&rule.to_string()).unwrap(), Some(rule));
        }
    }

    #[test]
    fn a_rule_that_moves_a_site_is_found() {
        let (mut rules, _) = rules_of("com\norg\nuk\nco.uk\ngov\n");
        rules.push(Rule::Subsite("blog.example.com".into()));
        let set: SiteRules = rules.iter().cloned().collect();
        let moved = moved_sites(
            &set,
            [
                "https://blog.example.com/a",
                "www.example.com",
                "news.bbc.co.uk",
                "news.ycombinator.com",
                "127.0.0.1",
            ],
        );
        assert_eq!(
            moved,
            [Moved {
                host: "blog.example.com".into(),
                built_in: Some("example.com".into()),
                by_rules: Some("blog.example.com".into()),
            }]
        );
    }

    /// The Public Suffix List the `psl` crate was built from, in the cargo
    /// registry: `psl` puts it there with the version in `Cargo.lock`.
    fn psl_rules_file() -> Option<std::path::PathBuf> {
        let lock = include_str!("../../../Cargo.lock");
        let at = lock.find("\nname = \"psl\"\nversion = \"")? + 25;
        let version = &lock[at..at + lock[at..].find('"')?];
        let home = std::env::var_os("CARGO_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| Some(std::path::PathBuf::from(std::env::var_os("HOME")?).join(".cargo")))?;
        std::fs::read_dir(home.join("registry/src"))
            .ok()?
            .filter_map(|entry| {
                let path = entry
                    .ok()?
                    .path()
                    .join(format!("psl-{version}/data/rules.txt"));
                path.exists().then_some(path)
            })
            .next()
    }

    /// Rules made from the suffix list the `psl` crate holds and the
    /// built-in lists give every host the site the built-in lists do, but
    /// where the `psl` crate departs from the list's own algorithm: a name
    /// a wildcard covers is a public suffix even when other rules name
    /// names under it (`*.run.app` makes `mtls.run.app` one, and
    /// `*.mtls.run.app` doesn't undo that), which the crate misses.
    #[test]
    fn rules_from_the_built_in_lists_move_no_site() {
        let Some(path) = psl_rules_file() else {
            eprintln!("no psl rules file in the cargo registry; nothing to compare");
            return;
        };
        let (rules, set) = rules_of(&std::fs::read_to_string(&path).unwrap());
        assert!(
            rules.len() > 9_000,
            "{} rules in {}",
            rules.len(),
            path.display()
        );
        let hosts = hosts_to_check(&rules);
        let wildcard_over_rules = |host: &str| {
            let covered = host
                .split_once('.')
                .is_some_and(|(_, parent)| rules.contains(&Rule::Wildcard(parent.to_string())));
            let under = format!(".{host}");
            covered
                && rules.iter().any(|rule| match rule {
                    Rule::Wildcard(name) => name == host || name.ends_with(&under),
                    Rule::Suffix(name) | Rule::Exception(name) => name.ends_with(&under),
                    _ => false,
                })
        };
        let moved: Vec<Moved> = moved_sites(&set, hosts.iter().map(String::as_str))
            .into_iter()
            .filter(|moved| moved.by_rules.is_some() || !wildcard_over_rules(&moved.host))
            .collect();
        assert!(
            moved.is_empty(),
            "{} of {} hosts move, such as {:?}",
            moved.len(),
            hosts.len(),
            &moved[..moved.len().min(10)]
        );
    }
}
