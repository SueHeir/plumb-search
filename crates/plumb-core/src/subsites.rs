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

/// The version of [`SUBDOMAIN_SITES`], [`SUBDOMAIN_SITE_NAMES`],
/// [`UMBRELLA_DOMAINS`] and of how seed data is read: bump it when they
/// change, so that nodes fold their seed data in again (version 0 is
/// before there were any lists; 2 refuses websites with user info, such as
/// `https://mailto:someone@gmail.com`, and adds the names).
pub const SITES_VERSION: u32 = 2;

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
const NOT_SITES: &[&str] = &["www", "m", "www2", "mobile"];

/// The site of `host` (lowercase, no trailing dot), whose registrable
/// domain is `domain`: a [`SUBDOMAIN_SITES`] entry it is or is under, the
/// direct subdomain of an [`UMBRELLA_DOMAINS`] entry it is or is under,
/// else `domain`.
pub(crate) fn site_of(host: &str, domain: &str) -> String {
    if host.len() == domain.len() {
        return domain.to_string();
    }
    let under = |site: &str| {
        host == site
            || host
                .strip_suffix(site)
                .is_some_and(|rest| rest.ends_with('.'))
    };
    if let Some(site) = SUBDOMAIN_SITES
        .iter()
        .filter(|site| under(site))
        .max_by_key(|site| site.len())
    {
        return (*site).to_string();
    }
    if UMBRELLA_DOMAINS.contains(&domain) {
        let rest = &host[..host.len() - domain.len() - 1];
        let label = rest.rsplit('.').next().unwrap_or(rest);
        if !label.is_empty() && !NOT_SITES.contains(&label) {
            return format!("{label}.{domain}");
        }
    }
    domain.to_string()
}

/// Each of the [`SUBDOMAIN_SITES`] with its registrable domain, whose
/// popularity ranks it gets: rank lists only name registrable domains.
pub fn subdomain_sites() -> impl Iterator<Item = (&'static str, &'static str)> {
    SUBDOMAIN_SITES
        .iter()
        .filter_map(|site| Some((*site, psl::domain_str(site)?)))
}

/// The registrable domain `site` is a subdomain of, when it is a site on
/// a subdomain (see [`crate::registrable_domain`]).
pub fn parent_domain(site: &str) -> Option<&str> {
    psl::domain_str(site).filter(|domain| *domain != site)
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
    fn the_lists_hold_valid_hosts() {
        for site in SUBDOMAIN_SITES {
            let domain = psl::domain_str(site).unwrap();
            assert_ne!(*site, domain, "{site} is not a subdomain");
            assert_eq!(site_of(site, domain), *site);
        }
        for (site, _) in SUBDOMAIN_SITE_NAMES {
            assert!(SUBDOMAIN_SITES.contains(site), "{site}");
        }
        for domain in UMBRELLA_DOMAINS {
            assert_eq!(psl::domain_str(domain), Some(*domain), "{domain}");
        }
    }
}
