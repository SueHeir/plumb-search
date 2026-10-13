//! Sites: which site a host belongs to.
//!
//! An item's `site` is the site of the host its URL is on, and a node
//! works it out itself, never taking it from the publisher. A site is the
//! host's registrable domain by the Public Suffix List (`news.bbc.co.uk`
//! is on `bbc.co.uk`), except for the few subdomains that are sites of
//! their own: Hacker News is `news.ycombinator.com`, and each direct
//! subdomain of a state portal such as `ca.gov` (`dmv.ca.gov`) is one.
//!
//! The rules are data, a set whose only kind is `site-rule`
//! ([`crate::Manifest::is_site_rules`]), so a change to them needs no
//! release. Each rule is an item whose id is the rule:
//!
//! | Id | Says |
//! |---|---|
//! | `suffix:co.uk` | `co.uk` is a public suffix |
//! | `suffix:*.ck` | every name directly under `ck` is a public suffix |
//! | `suffix:!www.ck` | but `www.ck` is not |
//! | `subsite:news.ycombinator.com` | that host, and every host under it, is a site of its own |
//! | `umbrella:ca.gov` | each direct subdomain of `ca.gov` is a site of its own |
//! | `not-site:www` | `www.ca.gov` stays `ca.gov`'s own |
//!
//! Names are in ASCII, internationalized ones in their `xn--` form. How
//! the rules apply is fixed here: the longest matching suffix wins, an
//! exception beats any other rule, and a name no rule covers has its last
//! label as its suffix, as the Public Suffix List's own algorithm says.

use std::collections::HashSet;
use std::fmt;

use anyhow::{bail, ensure, Result};

use crate::item::Item;
use crate::refs::is_domain;

/// The kind of every site rule, and the only kind of a site rules set.
pub const SITE_RULE: &str = "site-rule";

/// One site rule.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// A public suffix: `co.uk`.
    Suffix(String),
    /// Every name directly under this one is a public suffix: `ck` for
    /// `*.ck`.
    Wildcard(String),
    /// Not a public suffix, whatever a wildcard says: `www.ck` for
    /// `!www.ck`.
    Exception(String),
    /// A host that is a site of its own, with every host under it.
    Subsite(String),
    /// A domain whose every direct subdomain is a site of its own.
    Umbrella(String),
    /// A label that is never a site of its own under an umbrella.
    NotSite(String),
}

impl Rule {
    /// Reads a rule from an item's id. `Ok(None)` for a kind of rule this
    /// version does not know; an error for a known kind with a bad name.
    pub fn parse(id: &str) -> Result<Option<Rule>> {
        let Some((kind, name)) = id.split_once(':') else {
            bail!("site rule {id:?} is not <kind>:<name>");
        };
        let rule = match kind {
            "suffix" => {
                if let Some(rest) = name.strip_prefix("*.") {
                    Rule::Wildcard(rest.to_string())
                } else if let Some(rest) = name.strip_prefix('!') {
                    Rule::Exception(rest.to_string())
                } else {
                    Rule::Suffix(name.to_string())
                }
            }
            "subsite" => Rule::Subsite(name.to_string()),
            "umbrella" => Rule::Umbrella(name.to_string()),
            "not-site" => Rule::NotSite(name.to_string()),
            _ => return Ok(None),
        };
        match &rule {
            Rule::Suffix(name) | Rule::Wildcard(name) => ensure!(
                is_name(name),
                "site rule {id:?}: {name:?} is not a name in lower-case ASCII"
            ),
            Rule::Exception(name) => ensure!(
                is_name(name) && name.contains('.'),
                "site rule {id:?}: {name:?} is not a name under a suffix"
            ),
            Rule::Subsite(name) | Rule::Umbrella(name) => {
                ensure!(
                    is_domain(name),
                    "site rule {id:?}: {name:?} is not a domain"
                )
            }
            Rule::NotSite(label) => ensure!(
                is_name(label) && !label.contains('.'),
                "site rule {id:?}: {label:?} is not one label"
            ),
        }
        Ok(Some(rule))
    }

    /// Reads a site rules set's item as a rule, as [`Rule::parse`] does
    /// with its id.
    pub fn from_item(item: &Item) -> Result<Option<Rule>> {
        ensure!(
            item.kind == SITE_RULE,
            "item {:?} is a {:?}, not a site rule",
            item.id,
            item.kind
        );
        ensure!(!item.is_deleted(), "site rule {:?} is a tombstone", item.id);
        Rule::parse(&item.id)
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rule::Suffix(name) => write!(f, "suffix:{name}"),
            Rule::Wildcard(name) => write!(f, "suffix:*.{name}"),
            Rule::Exception(name) => write!(f, "suffix:!{name}"),
            Rule::Subsite(name) => write!(f, "subsite:{name}"),
            Rule::Umbrella(name) => write!(f, "umbrella:{name}"),
            Rule::NotSite(label) => write!(f, "not-site:{label}"),
        }
    }
}

/// A set of site rules, ready to say which site a host is.
#[derive(Clone, Debug, Default)]
pub struct SiteRules {
    suffixes: HashSet<String>,
    wildcards: HashSet<String>,
    exceptions: HashSet<String>,
    subsites: HashSet<String>,
    umbrellas: HashSet<String>,
    not_sites: HashSet<String>,
}

impl SiteRules {
    pub fn new() -> SiteRules {
        SiteRules::default()
    }

    pub fn add(&mut self, rule: Rule) {
        match rule {
            Rule::Suffix(name) => self.suffixes.insert(name),
            Rule::Wildcard(name) => self.wildcards.insert(name),
            Rule::Exception(name) => self.exceptions.insert(name),
            Rule::Subsite(name) => self.subsites.insert(name),
            Rule::Umbrella(name) => self.umbrellas.insert(name),
            Rule::NotSite(label) => self.not_sites.insert(label),
        };
    }

    /// Reads and adds the rules of a site rules set's items, passing over
    /// tombstones. Returns how many it added and, for each item it could not
    /// use, why.
    pub fn add_items<'a>(
        &mut self,
        items: impl IntoIterator<Item = &'a Item>,
    ) -> (usize, Vec<String>) {
        let (mut added, mut passed_over) = (0, Vec::new());
        for item in items {
            if item.is_deleted() {
                continue;
            }
            match Rule::from_item(item) {
                Ok(Some(rule)) => {
                    self.add(rule);
                    added += 1;
                }
                Ok(None) => passed_over.push(format!(
                    "site rule {:?} is newer than this version understands",
                    item.id
                )),
                Err(error) => passed_over.push(format!("{error:#}")),
            }
        }
        (added, passed_over)
    }

    /// How many rules it holds.
    pub fn len(&self) -> usize {
        self.suffixes.len()
            + self.wildcards.len()
            + self.exceptions.len()
            + self.subsites.len()
            + self.umbrellas.len()
            + self.not_sites.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The site of `host` (lower-case ASCII, no port, no trailing dot), or
    /// `None` when the host is itself a public suffix.
    pub fn site_of(&self, host: &str) -> Option<String> {
        let domain = self.registrable_domain(host)?;
        if host.len() == domain.len() {
            return Some(domain.to_string());
        }
        // The longest subsite the host is or is under.
        let mut name = host;
        loop {
            if self.subsites.contains(name) {
                return Some(name.to_string());
            }
            match name.split_once('.') {
                Some((_, rest)) if rest.len() > domain.len() => name = rest,
                _ => break,
            }
        }
        if self.umbrellas.contains(domain) {
            let rest = &host[..host.len() - domain.len() - 1];
            let label = rest.rsplit('.').next().unwrap_or(rest);
            if !label.is_empty() && !self.not_sites.contains(label) {
                return Some(format!("{label}.{domain}"));
            }
        }
        Some(domain.to_string())
    }

    /// The registrable domain of `host`: its public suffix and one label
    /// more, or `None` when the host is itself a public suffix.
    pub fn registrable_domain<'h>(&self, host: &'h str) -> Option<&'h str> {
        let rest = host.strip_suffix(self.suffix(host))?.strip_suffix('.')?;
        let start = rest.rfind('.').map_or(0, |dot| dot + 1);
        Some(&host[start..])
    }

    /// The public suffix of `host`: the longest name it ends in that a rule
    /// makes one, its parent when an exception names it, and its last label
    /// when no rule covers it. All of `host` when it is a public suffix.
    pub fn suffix<'h>(&self, host: &'h str) -> &'h str {
        let mut best = host.rfind('.').map_or(0, |dot| dot + 1);
        let mut parent: Option<&str> = None;
        let mut at = host.len();
        loop {
            let start = host[..at].rfind('.').map_or(0, |dot| dot + 1);
            let name = &host[start..];
            if self.exceptions.contains(name) {
                // Its parent: the name without its leftmost label.
                return host.get(at + 1..).unwrap_or("");
            }
            if self.suffixes.contains(name) || parent.is_some_and(|p| self.wildcards.contains(p)) {
                best = start;
            }
            if start == 0 {
                return &host[best..];
            }
            parent = Some(name);
            at = start - 1;
        }
    }

    /// The hosts that are sites of their own although they are subdomains.
    pub fn subsites(&self) -> impl Iterator<Item = &str> {
        self.subsites.iter().map(String::as_str)
    }
}

impl Extend<Rule> for SiteRules {
    fn extend<T: IntoIterator<Item = Rule>>(&mut self, rules: T) {
        for rule in rules {
            self.add(rule);
        }
    }
}

impl FromIterator<Rule> for SiteRules {
    fn from_iter<T: IntoIterator<Item = Rule>>(rules: T) -> SiteRules {
        let mut set = SiteRules::new();
        set.extend(rules);
        set
    }
}

/// The rules of a Public Suffix List file (`public_suffix_list.dat`): one
/// rule a line, `//` comments, names in Unicode or ASCII. Strict: a line it
/// cannot read is an error, so a list never quietly loses a rule.
pub fn read_suffix_list(text: &str) -> Result<Vec<Rule>> {
    let mut rules = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let Some(word) = line.split_whitespace().next() else {
            continue;
        };
        if word.starts_with("//") {
            continue;
        }
        let (mark, name) = if let Some(name) = word.strip_prefix("*.") {
            ("*.", name)
        } else if let Some(name) = word.strip_prefix('!') {
            ("!", name)
        } else {
            ("", word)
        };
        let ascii = match url::Host::parse(name) {
            Ok(url::Host::Domain(ascii)) => ascii,
            _ => bail!("line {}: {word:?} is not a domain", n + 1),
        };
        let rule = Rule::parse(&format!("suffix:{mark}{ascii}"))
            .map_err(|error| anyhow::anyhow!("line {}: {error:#}", n + 1))?
            .expect("suffix rules are known");
        rules.push(rule);
    }
    Ok(rules)
}

/// Lower-case ASCII labels of letters, digits and hyphens, none empty.
fn is_name(text: &str) -> bool {
    text.len() <= 253
        && text.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> SiteRules {
        let mut rules = SiteRules::new();
        for id in [
            "suffix:com",
            "suffix:uk",
            "suffix:co.uk",
            "suffix:io",
            "suffix:github.io",
            "suffix:ck",
            "suffix:*.ck",
            "suffix:!www.ck",
            "suffix:jp",
            "suffix:*.kawasaki.jp",
            "suffix:!city.kawasaki.jp",
            "suffix:cn",
            "suffix:xn--55qx5d.cn",
            "suffix:gov",
            "suffix:org",
            "subsite:news.ycombinator.com",
            "subsite:pubmed.ncbi.nlm.nih.gov",
            "subsite:ncbi.nlm.nih.gov",
            "subsite:docs.python.org",
            "umbrella:ca.gov",
            "umbrella:nih.gov",
            "not-site:www",
            "not-site:m",
        ] {
            rules.add(Rule::parse(id).unwrap().unwrap());
        }
        rules
    }

    #[test]
    fn rules_read_and_write() {
        for id in [
            "suffix:co.uk",
            "suffix:*.ck",
            "suffix:!www.ck",
            "suffix:com",
            "subsite:news.ycombinator.com",
            "umbrella:ca.gov",
            "not-site:www",
        ] {
            assert_eq!(Rule::parse(id).unwrap().unwrap().to_string(), id);
        }
        assert_eq!(Rule::parse("suffix-list:x").unwrap(), None);
        for bad in [
            "co.uk",
            "suffix:Co.uk",
            "suffix:公司.cn",
            "suffix:a..b",
            "suffix:!ck",
            "subsite:localhost",
            "umbrella:CA.gov",
            "not-site:www.x",
        ] {
            assert!(Rule::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn registrable_domains_follow_the_suffix_list() {
        let rules = rules();
        for (host, domain) in [
            ("example.com", Some("example.com")),
            ("www.example.com", Some("example.com")),
            ("news.bbc.co.uk", Some("bbc.co.uk")),
            ("co.uk", None),
            ("uk", None),
            ("x.github.io", Some("x.github.io")),
            ("a.x.github.io", Some("x.github.io")),
            ("github.io", None),
            ("a.b.ck", Some("a.b.ck")),
            ("b.ck", None),
            ("www.ck", Some("www.ck")),
            ("x.www.ck", Some("www.ck")),
            ("foo.bar.kawasaki.jp", Some("foo.bar.kawasaki.jp")),
            ("x.city.kawasaki.jp", Some("city.kawasaki.jp")),
            ("a.xn--55qx5d.cn", Some("a.xn--55qx5d.cn")),
            ("xn--55qx5d.cn", None),
            // No rule: the last label is the suffix.
            ("example.zzzunknown", Some("example.zzzunknown")),
            ("zzzunknown", None),
        ] {
            assert_eq!(rules.registrable_domain(host), domain, "{host}");
        }
    }

    #[test]
    fn suffixes_of_hosts() {
        let rules = rules();
        for (host, suffix) in [
            ("news.bbc.co.uk", "co.uk"),
            ("co.uk", "co.uk"),
            ("example.com", "com"),
            ("a.b.ck", "b.ck"),
            ("x.www.ck", "ck"),
            ("www.ck", "ck"),
            ("x.city.kawasaki.jp", "kawasaki.jp"),
            ("example.zzzunknown", "zzzunknown"),
            ("zzzunknown", "zzzunknown"),
        ] {
            assert_eq!(rules.suffix(host), suffix, "{host}");
        }
    }

    #[test]
    fn the_public_suffix_list_reads_as_rules() {
        let text = "// ===BEGIN ICANN DOMAINS===\n\nuk\nco.uk\n*.ck\n!www.ck\n\
                    公司.cn\nCOM // a note\n";
        let ids: Vec<String> = read_suffix_list(text)
            .unwrap()
            .iter()
            .map(Rule::to_string)
            .collect();
        assert_eq!(
            ids,
            [
                "suffix:uk",
                "suffix:co.uk",
                "suffix:*.ck",
                "suffix:!www.ck",
                "suffix:xn--55qx5d.cn",
                "suffix:com"
            ]
        );
        assert!(read_suffix_list("uk\na.*.uk\n").is_err());
        assert!(read_suffix_list("!uk\n").is_err());
        let rules: SiteRules = read_suffix_list(text).unwrap().into_iter().collect();
        assert_eq!(
            rules.site_of("a.xn--55qx5d.cn").as_deref(),
            Some("a.xn--55qx5d.cn")
        );
    }

    #[test]
    fn subdomains_that_are_sites_of_their_own() {
        let rules = rules();
        for (host, site) in [
            ("news.ycombinator.com", "news.ycombinator.com"),
            ("m.news.ycombinator.com", "news.ycombinator.com"),
            ("www.ycombinator.com", "ycombinator.com"),
            ("pubmed.ncbi.nlm.nih.gov", "pubmed.ncbi.nlm.nih.gov"),
            ("www.ncbi.nlm.nih.gov", "ncbi.nlm.nih.gov"),
            ("www.nlm.nih.gov", "nlm.nih.gov"),
            ("www.dmv.ca.gov", "dmv.ca.gov"),
            ("dmv.ca.gov", "dmv.ca.gov"),
            ("www.ca.gov", "ca.gov"),
            ("m.ca.gov", "ca.gov"),
            ("ca.gov", "ca.gov"),
            ("docs.python.org", "docs.python.org"),
            ("www.python.org", "python.org"),
        ] {
            assert_eq!(rules.site_of(host).as_deref(), Some(site), "{host}");
        }
        assert_eq!(rules.site_of("co.uk"), None);
    }

    #[test]
    fn a_set_of_rules_passes_over_what_it_cannot_read() {
        let item = |id: &str, kind: &str| -> Item {
            serde_json::from_value(serde_json::json!({"id": id, "kind": kind})).unwrap()
        };
        let items = [
            item("suffix:com", SITE_RULE),
            item("suffix:co.uk", SITE_RULE),
            item("prefix-site:x.com", SITE_RULE),
            item("suffix:CO.UK", SITE_RULE),
            item("suffix:org", "page"),
        ];
        let mut rules = SiteRules::new();
        let (added, passed_over) = rules.add_items(&items);
        assert_eq!((added, passed_over.len()), (2, 3), "{passed_over:?}");
        assert_eq!(rules.site_of("www.bbc.co.uk").as_deref(), Some("bbc.co.uk"));
    }
}
