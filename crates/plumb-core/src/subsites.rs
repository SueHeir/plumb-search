//! Sites that live on a subdomain.
//!
//! Plumb keeps one record per site, and a site is normally a registrable
//! domain: `www.usbank.com` and `usbank.com` are one site. A few well-known
//! sites are subdomains of someone else's domain, though: Hacker News is
//! `news.ycombinator.com`, Google Scholar `scholar.google.com`, Python's
//! docs `docs.python.org`. Folded into their parent domain they cannot be
//! found at all, and "hacker news" finds a look-alike instead.
//!
//! So two short lists, the same on every node, make subdomains sites:
//!
//! - [`SUBDOMAIN_SITES`]: hosts that are sites of their own, with every
//!   host under them (`www.`, `m.`) belonging to them.
//! - [`UMBRELLA_DOMAINS`]: domains whose every direct subdomain is a site of
//!   its own, such as a state government's `ca.gov`, where `dmv.ca.gov` is
//!   the DMV's site. `www.` stays the umbrella's own.
//!
//! A subdomain site inherits its parent domain's popularity ranks when the
//! seed records are built, as the rank lists only know registrable domains
//! (see [`subdomain_sites`]).
//!
//! These lists and the Public Suffix List are built in, as site rules
//! ([`built_in_rules`]), the same kinds of rule a site rules set the
//! project signs holds. A node may use such a set in their place
//! ([`use_site_rules`]), so that they change without a release.

use std::sync::{LazyLock, OnceLock};

use plumb_format::sites::{read_suffix_list, Rule};
use plumb_format::SiteRules;

/// The version of [`SUBDOMAIN_SITES`], [`SUBDOMAIN_SITE_NAMES`],
/// [`UMBRELLA_DOMAINS`] and of how seed data is read: bump it when they
/// change, so that nodes fold their seed data in again (version 0 is
/// before there were any lists; 2 refuses websites with user info, such as
/// `https://mailto:someone@gmail.com`, and adds the names; 3 gives a front
/// page that more items claim than one site carries to the item that owns
/// it, such as nba.com to the NBA over its seasons).
pub const SITES_VERSION: u32 = 3;

/// Names of [`SUBDOMAIN_SITES`] that the seed data may not name: Wikidata
/// gives Hacker News no official website the seed download keeps. A seed
/// with a record for the parent domain gets a record of these too.
pub const SUBDOMAIN_SITE_NAMES: &[(&str, &str)] = &[("news.ycombinator.com", "Hacker News")];

/// Hosts that are sites of their own although they are subdomains.
pub const SUBDOMAIN_SITES: &[&str] = &[
    "news.ycombinator.com",
    // Google's products.
    "mail.google.com",
    "scholar.google.com",
    "maps.google.com",
    "drive.google.com",
    "docs.google.com",
    "translate.google.com",
    "calendar.google.com",
    "photos.google.com",
    "play.google.com",
    "news.google.com",
    "books.google.com",
    "meet.google.com",
    "keep.google.com",
    "classroom.google.com",
    "earth.google.com",
    "trends.google.com",
    "cloud.google.com",
    "developers.google.com",
    "developer.android.com",
    // Microsoft's.
    "outlook.live.com",
    "onedrive.live.com",
    "teams.microsoft.com",
    "learn.microsoft.com",
    "portal.azure.com",
    // Apple's.
    "developer.apple.com",
    "support.apple.com",
    "music.apple.com",
    "apps.apple.com",
    // Yahoo's.
    "mail.yahoo.com",
    "finance.yahoo.com",
    "news.yahoo.com",
    "sports.yahoo.com",
    // Amazon's.
    "aws.amazon.com",
    // Developer documentation.
    "docs.python.org",
    "pypi.python.org",
    "developer.mozilla.org",
    "doc.rust-lang.org",
    "docs.github.com",
    "gist.github.com",
    "pkg.go.dev",
    // Messaging on the web.
    "web.whatsapp.com",
    "web.telegram.org",
    // Science and government.
    "pubmed.ncbi.nlm.nih.gov",
    "ncbi.nlm.nih.gov",
    "travel.state.gov",
];

/// Domains whose direct subdomains are each a site of their own: state
/// government portals, whose agencies each have a subdomain, and federal
/// agencies whose offices do.
pub const UMBRELLA_DOMAINS: &[&str] = &[
    // US states.
    "ca.gov",
    "ny.gov",
    "nj.gov",
    "pa.gov",
    "texas.gov",
    "virginia.gov",
    "illinois.gov",
    "michigan.gov",
    "ohio.gov",
    "georgia.gov",
    "nc.gov",
    "wa.gov",
    "az.gov",
    "mass.gov",
    "in.gov",
    "tn.gov",
    "mo.gov",
    "maryland.gov",
    "wisconsin.gov",
    "colorado.gov",
    "mn.gov",
    "sc.gov",
    "alabama.gov",
    "la.gov",
    "ky.gov",
    "oregon.gov",
    "ok.gov",
    "ct.gov",
    "utah.gov",
    "iowa.gov",
    "nv.gov",
    "arkansas.gov",
    "ms.gov",
    "ks.gov",
    "nm.gov",
    "nebraska.gov",
    "wv.gov",
    "idaho.gov",
    "hawaii.gov",
    "nh.gov",
    "maine.gov",
    "mt.gov",
    "ri.gov",
    "delaware.gov",
    "sd.gov",
    "nd.gov",
    "alaska.gov",
    "vermont.gov",
    "wyo.gov",
    "dc.gov",
    // US federal agencies with offices on subdomains.
    "noaa.gov",
    "nih.gov",
    "usda.gov",
    "nasa.gov",
];

/// Subdomains that are never a site of their own under an umbrella domain.
pub const NOT_SITES: &[&str] = &["www", "m", "www2", "mobile"];

/// The Public Suffix List, as the `psl` crate 2.1.238 holds it.
const SUFFIX_LIST: &str = include_str!("../data/public_suffix_list.dat");

/// The built-in lists as site rules ([`plumb_format::sites`]): the Public
/// Suffix List and [`list_rules`]. A site rules set the project signs
/// holds rules of the same kinds.
pub fn built_in_rules() -> Vec<Rule> {
    let mut rules = read_suffix_list(SUFFIX_LIST).expect("the built-in Public Suffix List reads");
    rules.extend(list_rules());
    rules
}

/// [`SUBDOMAIN_SITES`], [`UMBRELLA_DOMAINS`] and [`NOT_SITES`] as site
/// rules: the built-in lists but the Public Suffix List.
pub fn list_rules() -> Vec<Rule> {
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

static BUILT_IN: LazyLock<SiteRules> = LazyLock::new(|| built_in_rules().into_iter().collect());

static IN_USE: OnceLock<SiteRules> = OnceLock::new();

/// Makes `rules` say which site every host is from now on, in place of the
/// built-in ones, such as a site rules set the project signs, so that the
/// lists change without a release. Once only, at start, before any site is
/// worked out: false, leaving the rules in use, when others already are.
pub fn use_site_rules(rules: SiteRules) -> bool {
    IN_USE.set(rules).is_ok()
}

/// The rules in use: those [`use_site_rules`] put in place, else the
/// built-in ones.
pub(crate) fn rules() -> &'static SiteRules {
    // Not `unwrap_or`, which would read the built-in list for nothing.
    IN_USE.get().unwrap_or_else(|| &BUILT_IN)
}

/// The built-in rules, whatever [`use_site_rules`] put in their place.
pub(crate) fn built_in() -> &'static SiteRules {
    &BUILT_IN
}

/// Each of the sites on a subdomain with its registrable domain, whose
/// popularity ranks it gets: rank lists only name registrable domains.
/// In [`SUBDOMAIN_SITES`]' order while the built-in rules are in use, else
/// by name.
pub fn subdomain_sites() -> impl Iterator<Item = (&'static str, &'static str)> {
    let sites: Vec<&'static str> = match IN_USE.get() {
        Some(rules) => {
            let mut sites: Vec<&'static str> = rules.subsites().collect();
            sites.sort_unstable();
            sites
        }
        None => SUBDOMAIN_SITES.to_vec(),
    };
    sites
        .into_iter()
        .filter_map(|site| Some((site, domain_of(site)?)))
}

/// The registrable domain `site` is a subdomain of, when it is a site on
/// a subdomain (see [`crate::registrable_domain`]).
pub fn parent_domain(site: &str) -> Option<&str> {
    domain_of(site).filter(|domain| *domain != site)
}

/// The public suffix of `host` by the rules in use: `co.uk` for
/// `news.bbc.co.uk`, and all of it when it is one.
pub(crate) fn suffix_of(host: &str) -> &str {
    rules().suffix(host)
}

/// The registrable domain of `host`: its public suffix and one label more,
/// or `None` when it is a public suffix.
pub(crate) fn domain_of(host: &str) -> Option<&str> {
    rules().registrable_domain(host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{canonical_domain, registrable_domain};

    #[test]
    fn listed_subdomains_are_sites() {
        for (input, site) in [
            (
                "https://news.ycombinator.com/item?id=1",
                "news.ycombinator.com",
            ),
            ("news.ycombinator.com", "news.ycombinator.com"),
            ("https://www.ycombinator.com/", "ycombinator.com"),
            ("https://scholar.google.com/", "scholar.google.com"),
            ("https://www.google.com/search", "google.com"),
            ("https://accounts.google.com/", "google.com"),
            ("https://docs.python.org/3/", "docs.python.org"),
            ("https://www.python.org/", "python.org"),
            (
                "https://pubmed.ncbi.nlm.nih.gov/123/",
                "pubmed.ncbi.nlm.nih.gov",
            ),
            ("https://www.ncbi.nlm.nih.gov/", "ncbi.nlm.nih.gov"),
            ("https://news.bbc.co.uk/", "bbc.co.uk"),
        ] {
            assert_eq!(registrable_domain(input).as_deref(), Some(site), "{input}");
            assert_eq!(canonical_domain(site).as_deref(), Some(site), "{site}");
        }
    }

    #[test]
    fn umbrella_subdomains_are_sites() {
        for (input, site) in [
            ("https://www.dmv.ca.gov/portal/", "dmv.ca.gov"),
            ("https://dmv.ca.gov/", "dmv.ca.gov"),
            ("https://www.ca.gov/", "ca.gov"),
            ("https://ca.gov/", "ca.gov"),
            ("https://www.nhc.noaa.gov/", "nhc.noaa.gov"),
            ("https://forecast.nhc.noaa.gov/", "nhc.noaa.gov"),
            ("https://m.nasa.gov/", "nasa.gov"),
            ("https://www.nlm.nih.gov/", "nlm.nih.gov"),
        ] {
            assert_eq!(registrable_domain(input).as_deref(), Some(site), "{input}");
        }
    }

    #[test]
    fn subdomain_sites_know_their_domain() {
        let parents: Vec<(&str, &str)> = subdomain_sites().collect();
        assert_eq!(parents.len(), SUBDOMAIN_SITES.len());
        assert!(parents.contains(&("scholar.google.com", "google.com")));
        assert!(parents.contains(&("news.ycombinator.com", "ycombinator.com")));
        assert_eq!(parent_domain("dmv.ca.gov"), Some("ca.gov"));
        assert_eq!(parent_domain("ca.gov"), None);
        assert_eq!(parent_domain("usbank.com"), None);
    }

    #[test]
    fn the_built_in_rules_read_back_as_rules() {
        for rule in built_in_rules() {
            assert_eq!(Rule::parse(&rule.to_string()).unwrap(), Some(rule));
        }
    }

    #[test]
    fn the_lists_hold_valid_hosts() {
        for site in SUBDOMAIN_SITES {
            let domain = built_in().registrable_domain(site).unwrap();
            assert_ne!(*site, domain, "{site} is not a subdomain");
            assert_eq!(built_in().site_of(site).as_deref(), Some(*site));
        }
        for (site, _) in SUBDOMAIN_SITE_NAMES {
            assert!(SUBDOMAIN_SITES.contains(site), "{site}");
        }
        for domain in UMBRELLA_DOMAINS {
            assert_eq!(
                built_in().registrable_domain(domain),
                Some(*domain),
                "{domain}"
            );
        }
    }

    /// The built-in list gives every host the registrable domain the `psl`
    /// crate it was taken from does, but where the crate departs from the
    /// list's own algorithm: a name a wildcard covers is a public suffix
    /// even when other rules name names under it (`*.run.app` makes
    /// `mtls.run.app` one, and `*.mtls.run.app` doesn't undo that), which
    /// the crate misses.
    #[test]
    fn the_built_in_list_is_the_psl_crates() {
        let rules = read_suffix_list(SUFFIX_LIST).unwrap();
        assert!(rules.len() > 9_000, "{} rules", rules.len());
        let named = |wanted: fn(&Rule) -> Option<&str>| -> std::collections::HashSet<&str> {
            rules.iter().filter_map(wanted).collect()
        };
        let wildcards = named(|rule| match rule {
            Rule::Wildcard(name) => Some(name.as_str()),
            _ => None,
        });
        let suffixes = named(|rule| match rule {
            Rule::Suffix(name) => Some(name.as_str()),
            _ => None,
        });
        let mut hosts = Vec::new();
        for rule in &rules {
            let (Rule::Suffix(name) | Rule::Wildcard(name) | Rule::Exception(name)) = rule else {
                continue;
            };
            for host in [name.clone(), format!("a.{name}"), format!("b.a.{name}")] {
                hosts.push(host);
            }
        }
        let (mut checked, mut excused) = (0, 0);
        for host in &hosts {
            let ours = built_in().registrable_domain(host);
            let theirs = psl::domain_str(host);
            if ours == theirs {
                checked += 1;
                continue;
            }
            // The crate's miss: the host is, or is under, a suffix that a
            // wildcard covers and a rule of its own names too
            // (`*.mtls.run.app` beside `*.run.app`). The list's rules say
            // such a suffix has no registrable domain; the crate gives it
            // one.
            let mut name = host.as_str();
            let mut missed = false;
            loop {
                let parent = name.split_once('.').map(|(_, parent)| parent);
                let own_rule = suffixes.contains(name) || wildcards.contains(name);
                if own_rule && parent.is_some_and(|p| wildcards.contains(p)) {
                    missed = true;
                    break;
                }
                let Some(parent) = parent else { break };
                name = parent;
            }
            assert!(missed, "{host}: {ours:?}, the psl crate {theirs:?}");
            excused += 1;
        }
        // 21 in the list as vendored.
        assert!(excused < 30, "{excused} hosts differ");
        assert!(checked > 30_000, "{checked} of {} hosts", hosts.len());
    }
}
