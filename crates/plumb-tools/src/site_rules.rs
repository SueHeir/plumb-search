//! Site rules beyond the built-in lists, and checking a site rules set
//! before it is signed: the hosts whose site it would move from the one
//! the built-in lists give.

use std::collections::HashMap;
use std::io::BufRead;

use anyhow::{Context, Result};
use plumb_core::subsites::NOT_SITES;
use plumb_core::{is_locale_segment, reverse_host};
use plumb_format::sites::Rule;
use plumb_format::SiteRules;

/// A domain with this many subdomains named like a language (`en.`,
/// `de.`) among the hosts read is a site in many languages, and those are
/// its copies, not sites of their own.
const LANGUAGE_COPIES: usize = 3;

/// A subsite rule for each direct subdomain among the first `top` hosts of
/// a Common Crawl host ranks file (`<release>-host-ranks.txt`, best linked
/// first, hosts reversed in its `#host_rev` column) that `rules` folds into
/// its domain: services the web links to as sites of their own, such as
/// `meteoswiss.admin.ch` or `msrc.microsoft.com`. A `www.` is taken off
/// first. Passed over: a deeper host (its subdomain may be the site), a
/// mirror (`m.`, `www3.`), a name of one letter or no letters, which holds
/// images and files (`1.bp.blogspot.com`, `i.example.com`), and a language
/// copy of a site in many languages (`de.wikipedia.org`, [`LANGUAGE_COPIES`]).
pub fn well_linked_subsites(
    ranks: impl BufRead,
    top: usize,
    rules: &SiteRules,
) -> Result<Vec<Rule>> {
    let mut lines = ranks.lines();
    let header = lines.next().transpose()?.unwrap_or_default();
    let column = header
        .split('\t')
        .position(|name| name.trim().trim_start_matches('#') == "host_rev")
        .context("no #host_rev column in the host ranks")?;
    // Each host with its domain, and whether it is named like a language.
    let mut found: Vec<(String, String, bool)> = Vec::new();
    for line in lines.take(top) {
        let line = line?;
        let Some(reversed) = line.split('\t').nth(column) else {
            continue;
        };
        let host = reverse_host(reversed).to_ascii_lowercase();
        let host = host.strip_prefix("www.").unwrap_or(&host);
        let Some(domain) = rules.registrable_domain(host) else {
            continue;
        };
        let Some(label) = host
            .strip_suffix(domain)
            .and_then(|rest| rest.strip_suffix('.'))
        else {
            continue;
        };
        let mirror = NOT_SITES.contains(&label)
            || label
                .strip_prefix("www")
                .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()));
        let named = label.len() > 1 && label.bytes().any(|b| b.is_ascii_lowercase());
        if named
            && !mirror
            && !label.contains('.')
            && rules.site_of(host).as_deref() == Some(domain)
        {
            found.push((
                host.to_string(),
                domain.to_string(),
                is_locale_segment(label),
            ));
        }
    }
    let mut languages: HashMap<&str, usize> = HashMap::new();
    for (_, domain, _) in found.iter().filter(|(_, _, language)| *language) {
        *languages.entry(domain).or_default() += 1;
    }
    let mut subsites: Vec<Rule> = found
        .iter()
        .filter(|(_, domain, language)| {
            !language || languages.get(domain.as_str()).copied().unwrap_or(0) < LANGUAGE_COPIES
        })
        .map(|(host, _, _)| Rule::Subsite(host.clone()))
        .collect();
    subsites.sort_by_key(ToString::to_string);
    subsites.dedup();
    Ok(subsites)
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
    use plumb_node::site_rules::built_in_rules;

    fn rules_of(text: &str) -> (Vec<Rule>, SiteRules) {
        let mut rules = read_suffix_list(text).unwrap();
        rules.extend(built_in_rules());
        let set = rules.iter().cloned().collect();
        (rules, set)
    }

    #[test]
    fn well_linked_subdomains_become_sites() {
        let (_, set) = rules_of("com\norg\nch\ngov\n");
        let mut ranks =
            String::from("#harmonicc_pos\t#harmonicc_val\t#pr_pos\t#pr_val\t#host_rev\n");
        for host in [
            "com.microsoft.msrc",
            "ch.admin.meteoswiss.www",
            "com.example.www",
            "com.example.m",
            "com.example.www3",
            "com.blogspot.bp.1",
            "com.example.i",
            "com.example.blogs.technet",
            "com.google.scholar",
            "org.wikipedia.en",
            "org.wikipedia.de",
            "org.wikipedia.fr",
            "org.europa.ec",
            "com.example.late",
        ] {
            ranks.push_str(&format!("1\t1\t1\t1\t{host}\n"));
        }
        let rules = well_linked_subsites(ranks.as_bytes(), 13, &set).unwrap();
        let hosts: Vec<String> = rules.iter().map(|rule| rule.to_string()).collect();
        let subsites: Vec<Rule> = ["ec.europa.org", "meteoswiss.admin.ch", "msrc.microsoft.com"]
            .into_iter()
            .map(|host| Rule::Subsite(host.into()))
            .collect();
        assert_eq!(rules, subsites, "{hosts:?}");
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
