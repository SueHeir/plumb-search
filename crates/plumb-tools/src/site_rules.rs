//! Checking a site rules set before it is signed: the hosts whose site
//! it would move from the one the built-in lists give.

use plumb_format::sites::Rule;
use plumb_format::SiteRules;

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
