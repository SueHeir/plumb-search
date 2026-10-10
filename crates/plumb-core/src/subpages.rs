//! Subpage sites: well-known sites beyond software docs and reference
//! sites whose inner pages people search for by name: a university's
//! departments and labs, a professor's or a lab's own pages, a big
//! company's products and investor pages, a government agency's
//! publications, a team's or a show's pages, a museum's collections and
//! visiting hours. Their pages make the `subpages` page set and are found
//! like reference pages ([`crate::reference`]): by their whole title
//! ("Perft Results", "Special Publication 811") or by most of a search's
//! words.
//!
//! Each site's pages are those its sitemaps list under its roots, and the
//! pages its roots (its homepage or section pages) and index pages link to
//! under them, so a site with no sitemap still gives its main pages. The
//! shallowest are taken first, up to a cap per site, so the set stays
//! small: about 200,000 pages in all.

use crate::reference::ReferenceSite;

/// What a subpage site is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubpageKind {
    /// Universities, their departments and labs, and researchers' pages.
    University,
    /// Big companies.
    Company,
    /// Government agencies and standards bodies.
    Government,
    /// Films, TV, music, games and sports.
    Entertainment,
    /// Museums.
    Museum,
}

impl SubpageKind {
    /// Its name, for `--subpage-kinds` and logs.
    pub fn name(self) -> &'static str {
        match self {
            SubpageKind::University => "university",
            SubpageKind::Company => "company",
            SubpageKind::Government => "government",
            SubpageKind::Entertainment => "entertainment",
            SubpageKind::Museum => "museum",
        }
    }

    /// The kind named `name`.
    pub fn of_name(name: &str) -> Option<Self> {
        [
            SubpageKind::University,
            SubpageKind::Company,
            SubpageKind::Government,
            SubpageKind::Entertainment,
            SubpageKind::Museum,
        ]
        .into_iter()
        .find(|kind| kind.name() == name)
    }
}

/// A site whose inner pages are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubpageSite {
    pub kind: SubpageKind,
    /// Its host, roots, sitemaps, weight and cap, as a reference site's.
    pub site: ReferenceSite,
    /// Pages linking to its pages besides its roots: a wiki's list of all
    /// its pages.
    pub index_pages: &'static [&'static str],
}

impl SubpageSite {
    /// Its host without `www.`, as the site is keyed.
    pub fn key(&self) -> &'static str {
        self.site.key()
    }

    /// Where its pages are: its roots, each also on its host without
    /// `www.` (or with it, for a registrable domain), since many sites link
    /// to both ("norvig.com" and "www.norvig.com").
    pub fn roots(&self) -> Vec<String> {
        let mut roots = self.site.roots();
        for root in self.site.roots() {
            let Some(rest) = root.strip_prefix("https://") else {
                continue;
            };
            let host = rest.split('/').next().unwrap_or(rest);
            match rest.strip_prefix("www.") {
                Some(bare) => roots.push(format!("https://{bare}")),
                // Only a registrable domain has a `www.` twin, not
                // "nssdc.gsfc.nasa.gov".
                None if host.matches('.').count() == 1 => roots.push(format!("https://www.{rest}")),
                None => {}
            }
        }
        roots
    }

    /// The pages read for links to its pages: its roots, then its index
    /// pages.
    pub fn index_pages(&self) -> Vec<String> {
        let mut pages = self.site.roots();
        pages.extend(self.index_pages.iter().map(|page| page.to_string()));
        pages
    }

    /// Most pages fetched: its own cap, else [`DEFAULT_MAX_PAGES`].
    pub fn max_pages(&self) -> usize {
        self.site.max_pages.unwrap_or(DEFAULT_MAX_PAGES)
    }
}

/// Most pages fetched of a site without a cap of its own.
pub const DEFAULT_MAX_PAGES: usize = 1_000;

/// A site of `kind` and `weight` whose pages are anywhere on `host`.
const fn whole(kind: SubpageKind, host: &'static str, weight: u64) -> SubpageSite {
    sized(kind, host, &[], weight, None)
}

/// A site of `kind` and `weight` whose pages are under `roots`.
const fn under(
    kind: SubpageKind,
    host: &'static str,
    roots: &'static [&'static str],
    weight: u64,
) -> SubpageSite {
    sized(kind, host, roots, weight, None)
}

/// A site of `kind` and `weight` whose pages are under `roots` (anywhere
/// when empty), up to `max_pages` of them.
const fn sized(
    kind: SubpageKind,
    host: &'static str,
    roots: &'static [&'static str],
    weight: u64,
    max_pages: Option<usize>,
) -> SubpageSite {
    SubpageSite {
        kind,
        site: ReferenceSite {
            host,
            roots,
            sitemaps: &[],
            index_pages: &[],
            weight,
            max_pages,
        },
        index_pages: &[],
    }
}

/// A site of `kind` and `weight` with up to `max_pages` pages anywhere on
/// `host`.
const fn big(kind: SubpageKind, host: &'static str, weight: u64, max_pages: usize) -> SubpageSite {
    sized(kind, host, &[], weight, Some(max_pages))
}

/// A wiki of `kind` and `weight` whose pages are all listed on
/// `all_pages`, up to `max_pages` of them.
const fn wiki(
    kind: SubpageKind,
    host: &'static str,
    roots: &'static [&'static str],
    all_pages: &'static [&'static str],
    weight: u64,
    max_pages: usize,
) -> SubpageSite {
    let mut site = sized(kind, host, roots, weight, Some(max_pages));
    site.index_pages = all_pages;
    site
}

use SubpageKind::{Company, Entertainment, Government, Museum, University};

/// The subpage sites, by kind, the most used of each first.
pub const SUBPAGE_SITES: &[SubpageSite] = &[
    // Universities: their main pages (schools, departments, admissions).
    big(University, "www.mit.edu", 7, 2_000),
    big(University, "www.stanford.edu", 7, 2_000),
    big(University, "www.harvard.edu", 7, 2_000),
    big(University, "www.berkeley.edu", 6, 2_000),
    big(University, "www.cmu.edu", 6, 2_000),
    big(University, "www.caltech.edu", 6, 2_000),
    big(University, "www.princeton.edu", 6, 2_000),
    big(University, "www.yale.edu", 6, 2_000),
    big(University, "www.columbia.edu", 6, 2_000),
    big(University, "www.uchicago.edu", 5, 2_000),
    big(University, "www.upenn.edu", 5, 2_000),
    big(University, "www.cornell.edu", 5, 2_000),
    big(University, "www.ucla.edu", 5, 2_000),
    big(University, "umich.edu", 5, 2_000),
    big(University, "www.washington.edu", 5, 2_000),
    big(University, "www.utexas.edu", 5, 2_000),
    big(University, "www.gatech.edu", 5, 2_000),
    big(University, "illinois.edu", 5, 2_000),
    big(University, "www.nyu.edu", 5, 2_000),
    big(University, "www.jhu.edu", 5, 2_000),
    big(University, "www.ox.ac.uk", 6, 2_000),
    big(University, "www.cam.ac.uk", 6, 2_000),
    big(University, "www.imperial.ac.uk", 5, 2_000),
    big(University, "www.ucl.ac.uk", 5, 2_000),
    big(University, "ethz.ch", 5, 2_000),
    big(University, "www.utoronto.ca", 5, 2_000),
    // Departments and labs: their people, research groups and courses.
    big(University, "www.csail.mit.edu", 6, 3_000),
    big(University, "www.media.mit.edu", 5, 2_000),
    big(University, "news.mit.edu", 5, 2_000),
    big(University, "ocw.mit.edu", 6, 3_000),
    big(University, "www.cs.stanford.edu", 5, 2_000),
    big(University, "ai.stanford.edu", 5, 1_000),
    big(University, "hai.stanford.edu", 4, 1_000),
    big(University, "eecs.berkeley.edu", 5, 2_000),
    big(University, "bair.berkeley.edu", 4, 1_000),
    big(University, "www.cs.cmu.edu", 5, 3_000),
    big(University, "www.ri.cmu.edu", 4, 1_000),
    big(University, "www.cs.princeton.edu", 4, 1_000),
    big(University, "www.cs.washington.edu", 4, 1_000),
    big(University, "seas.harvard.edu", 4, 1_000),
    big(University, "web.cs.toronto.edu", 4, 1_000),
    big(University, "www.cs.ox.ac.uk", 4, 1_000),
    big(University, "www.cst.cam.ac.uk", 4, 1_000),
    big(University, "www.ias.edu", 4, 1_000),
    big(University, "home.cern", 5, 2_000),
    big(University, "www.llnl.gov", 4, 1_000),
    big(University, "www.lanl.gov", 4, 1_000),
    big(University, "www.ornl.gov", 4, 1_000),
    big(University, "www.jpl.nasa.gov", 5, 2_000),
    // Researchers' own pages and the research wikis they keep.
    whole(University, "www.norvig.com", 4),
    under(
        University,
        "www-cs-faculty.stanford.edu",
        &["https://www-cs-faculty.stanford.edu/~knuth/"],
        4,
    ),
    whole(University, "www.paulgraham.com", 4),
    whole(University, "www.scottaaronson.com", 3),
    whole(University, "terrytao.wordpress.com", 3),
    wiki(
        University,
        "chessprogramming.org",
        &[],
        &["https://chessprogramming.org/Special:AllPages"],
        4,
        5_000,
    ),
    wiki(
        University,
        "conwaylife.com",
        &["https://conwaylife.com/wiki/"],
        &["https://conwaylife.com/wiki/Special:AllPages"],
        3,
        3_000,
    ),
    big(University, "mathworld.wolfram.com", 6, 5_000),
    big(University, "oeis.org", 4, 1_000),
    big(University, "plato.stanford.edu", 6, 3_000),
    big(University, "iep.utm.edu", 4, 1_000),
    big(University, "www.feynmanlectures.caltech.edu", 4, 1_000),
    big(University, "hyperphysics.phy-astr.gsu.edu", 4, 2_000),
    // Big companies: products, careers, investors, newsrooms.
    big(Company, "www.apple.com", 8, 2_000),
    big(Company, "www.microsoft.com", 8, 2_000),
    big(Company, "about.google", 7, 1_000),
    big(Company, "blog.google", 6, 1_000),
    big(Company, "store.google.com", 6, 1_000),
    big(Company, "www.aboutamazon.com", 6, 1_000),
    under(Company, "www.meta.com", &["https://www.meta.com/about/"], 6),
    big(Company, "www.tesla.com", 7, 1_000),
    big(Company, "www.nvidia.com", 7, 2_000),
    big(Company, "www.ibm.com", 6, 2_000),
    big(Company, "www.intel.com", 6, 2_000),
    big(Company, "www.amd.com", 6, 1_000),
    big(Company, "www.samsung.com", 7, 2_000),
    big(Company, "www.sony.com", 5, 1_000),
    big(Company, "www.dell.com", 5, 1_000),
    big(Company, "www.hp.com", 5, 1_000),
    big(Company, "www.lenovo.com", 5, 1_000),
    big(Company, "www.cisco.com", 5, 1_000),
    big(Company, "www.oracle.com", 5, 2_000),
    big(Company, "www.salesforce.com", 5, 1_000),
    big(Company, "www.adobe.com", 6, 1_000),
    big(Company, "openai.com", 6, 1_000),
    big(Company, "www.anthropic.com", 5, 1_000),
    big(Company, "about.netflix.com", 5, 1_000),
    big(Company, "thewaltdisneycompany.com", 5, 1_000),
    big(Company, "www.coca-colacompany.com", 5, 1_000),
    big(Company, "about.nike.com", 5, 1_000),
    big(Company, "corporate.walmart.com", 5, 1_000),
    big(Company, "corporate.target.com", 5, 1_000),
    big(Company, "corporate.mcdonalds.com", 5, 1_000),
    big(Company, "www.starbucks.com", 5, 1_000),
    big(Company, "www.costco.com", 5, 1_000),
    big(Company, "www.ford.com", 5, 1_000),
    big(Company, "www.toyota.com", 5, 1_000),
    big(Company, "www.gm.com", 5, 1_000),
    big(Company, "www.boeing.com", 5, 1_000),
    big(Company, "www.spacex.com", 5, 500),
    big(Company, "www.jpmorganchase.com", 5, 1_000),
    big(Company, "www.berkshirehathaway.com", 4, 500),
    big(Company, "www.pfizer.com", 5, 1_000),
    big(Company, "www.jnj.com", 4, 1_000),
    big(Company, "us.pg.com", 4, 1_000),
    big(Company, "www.unilever.com", 4, 1_000),
    big(Company, "www.shell.com", 4, 1_000),
    big(Company, "corporate.exxonmobil.com", 4, 1_000),
    big(Company, "www.att.com", 4, 1_000),
    big(Company, "www.verizon.com", 4, 1_000),
    // Government agencies and standards bodies: their programs,
    // publications and data (the reference set has the IRS, SSA, NPS and
    // other services people use).
    big(Government, "www.nist.gov", 6, 5_000),
    under(
        Government,
        "physics.nist.gov",
        &["https://physics.nist.gov/cuu/"],
        4,
    ),
    big(Government, "www.noaa.gov", 6, 2_000),
    big(Government, "www.weather.gov", 6, 2_000),
    big(Government, "www.nih.gov", 6, 2_000),
    big(Government, "www.nsf.gov", 5, 2_000),
    big(Government, "www.census.gov", 6, 3_000),
    big(Government, "www.whitehouse.gov", 6, 1_000),
    big(Government, "www.congress.gov", 6, 2_000),
    big(Government, "www.senate.gov", 5, 1_000),
    big(Government, "www.house.gov", 5, 1_000),
    big(Government, "www.supremecourt.gov", 5, 1_000),
    big(Government, "www.state.gov", 5, 2_000),
    big(Government, "www.defense.gov", 5, 1_000),
    big(Government, "www.energy.gov", 5, 2_000),
    big(Government, "www.usgs.gov", 6, 3_000),
    big(Government, "www.fbi.gov", 5, 1_000),
    big(Government, "www.cia.gov", 5, 1_000),
    big(Government, "www.ftc.gov", 5, 1_000),
    big(Government, "www.sec.gov", 5, 2_000),
    big(Government, "www.fcc.gov", 5, 1_000),
    big(Government, "www.loc.gov", 6, 3_000),
    big(Government, "www.archives.gov", 6, 3_000),
    big(Government, "www.justice.gov", 5, 1_000),
    big(Government, "www.dhs.gov", 4, 1_000),
    big(Government, "www.transportation.gov", 4, 1_000),
    big(Government, "www.faa.gov", 5, 2_000),
    big(Government, "www.osha.gov", 5, 1_000),
    big(Government, "www.ed.gov", 4, 1_000),
    big(Government, "www.hud.gov", 4, 1_000),
    big(Government, "www.usda.gov", 5, 1_000),
    big(Government, "science.nasa.gov", 6, 3_000),
    under(
        Government,
        "nssdc.gsfc.nasa.gov",
        &["https://nssdc.gsfc.nasa.gov/planetary/"],
        5,
    ),
    big(Government, "www.bipm.org", 5, 1_000),
    big(Government, "ciaaw.org", 4, 500),
    big(Government, "www.iso.org", 5, 1_000),
    big(Government, "www.legislation.gov.uk", 5, 2_000),
    big(Government, "www.parliament.uk", 5, 1_000),
    big(Government, "www.ons.gov.uk", 5, 1_000),
    big(Government, "european-union.europa.eu", 5, 1_000),
    big(Government, "www.un.org", 6, 2_000),
    big(Government, "www.who.int", 6, 2_000),
    big(Government, "www.imf.org", 4, 1_000),
    big(Government, "www.worldbank.org", 4, 1_000),
    // Entertainment: films and shows, music, games and sports.
    big(Entertainment, "www.rottentomatoes.com", 6, 5_000),
    big(Entertainment, "www.boxofficemojo.com", 5, 2_000),
    big(Entertainment, "www.metacritic.com", 5, 3_000),
    big(Entertainment, "www.billboard.com", 5, 2_000),
    big(Entertainment, "www.rollingstone.com", 4, 2_000),
    big(Entertainment, "www.grammy.com", 4, 1_000),
    big(Entertainment, "www.oscars.org", 4, 1_000),
    big(Entertainment, "www.televisionacademy.com", 4, 1_000),
    big(Entertainment, "www.ibdb.com", 4, 2_000),
    big(Entertainment, "www.broadway.org", 4, 1_000),
    big(Entertainment, "www.marvel.com", 5, 2_000),
    big(Entertainment, "www.starwars.com", 5, 2_000),
    big(Entertainment, "www.dc.com", 4, 1_000),
    big(Entertainment, "www.disneyplus.com", 5, 1_000),
    big(Entertainment, "www.hbo.com", 5, 2_000),
    big(Entertainment, "www.nbc.com", 4, 1_000),
    big(Entertainment, "www.cbs.com", 4, 1_000),
    big(Entertainment, "www.pbs.org", 5, 2_000),
    big(Entertainment, "www.nintendo.com", 5, 2_000),
    big(Entertainment, "www.playstation.com", 5, 2_000),
    big(Entertainment, "www.xbox.com", 5, 1_000),
    big(Entertainment, "www.ign.com", 5, 3_000),
    big(Entertainment, "www.gamespot.com", 4, 2_000),
    big(Entertainment, "www.minecraft.net", 5, 1_000),
    big(Entertainment, "www.nba.com", 6, 3_000),
    big(Entertainment, "www.nfl.com", 6, 3_000),
    big(Entertainment, "www.mlb.com", 6, 3_000),
    big(Entertainment, "www.nhl.com", 5, 2_000),
    big(Entertainment, "www.espn.com", 6, 3_000),
    big(Entertainment, "www.fifa.com", 5, 2_000),
    big(Entertainment, "www.premierleague.com", 5, 2_000),
    big(Entertainment, "www.uefa.com", 5, 2_000),
    big(Entertainment, "www.olympics.com", 5, 3_000),
    big(Entertainment, "www.formula1.com", 5, 2_000),
    big(Entertainment, "www.pgatour.com", 4, 1_000),
    big(Entertainment, "www.wimbledon.com", 4, 1_000),
    big(Entertainment, "www.ncaa.com", 5, 2_000),
    big(Entertainment, "www.wwe.com", 4, 1_000),
    big(Entertainment, "disneyworld.disney.go.com", 5, 2_000),
    big(Entertainment, "disneyland.disney.go.com", 5, 1_000),
    big(Entertainment, "www.universalorlando.com", 4, 1_000),
    // Museums: exhibitions, collections, visiting.
    big(Museum, "www.metmuseum.org", 6, 3_000),
    big(Museum, "www.moma.org", 5, 2_000),
    big(Museum, "www.louvre.fr", 5, 2_000),
    big(Museum, "www.britishmuseum.org", 5, 2_000),
    big(Museum, "www.si.edu", 6, 2_000),
    big(Museum, "naturalhistory.si.edu", 5, 1_000),
    big(Museum, "airandspace.si.edu", 5, 2_000),
    big(Museum, "americanhistory.si.edu", 5, 1_000),
    big(Museum, "nmaahc.si.edu", 4, 1_000),
    big(Museum, "americanindian.si.edu", 4, 1_000),
    big(Museum, "www.nga.gov", 5, 2_000),
    big(Museum, "www.guggenheim.org", 4, 1_000),
    big(Museum, "www.artic.edu", 5, 2_000),
    big(Museum, "www.getty.edu", 5, 2_000),
    big(Museum, "www.tate.org.uk", 5, 2_000),
    big(Museum, "www.vam.ac.uk", 5, 2_000),
    big(Museum, "www.nhm.ac.uk", 5, 2_000),
    big(Museum, "www.sciencemuseum.org.uk", 4, 1_000),
    big(Museum, "www.nationalgallery.org.uk", 5, 1_000),
    big(Museum, "www.rijksmuseum.nl", 5, 1_000),
    big(Museum, "www.vangoghmuseum.nl", 4, 1_000),
    big(Museum, "www.museodelprado.es", 4, 1_000),
    big(Museum, "www.uffizi.it", 4, 1_000),
    big(Museum, "www.hermitagemuseum.org", 3, 500),
    big(Museum, "www.amnh.org", 5, 2_000),
    big(Museum, "www.fieldmuseum.org", 4, 1_000),
    big(Museum, "www.exploratorium.edu", 4, 1_000),
    big(Museum, "whitney.org", 4, 1_000),
    big(Museum, "www.lacma.org", 4, 1_000),
    big(Museum, "www.sfmoma.org", 4, 1_000),
    big(Museum, "www.ushmm.org", 4, 1_000),
    big(Museum, "www.911memorial.org", 4, 500),
    big(Museum, "computerhistory.org", 4, 1_000),
    big(Museum, "www.thehenryford.org", 4, 1_000),
    big(Museum, "www.mfa.org", 4, 1_000),
    big(Museum, "www.philamuseum.org", 4, 1_000),
    big(Museum, "www.clevelandart.org", 4, 1_000),
    big(Museum, "www.brooklynmuseum.org", 4, 1_000),
    big(Museum, "www.frick.org", 3, 500),
    big(Museum, "www.rom.on.ca", 4, 1_000),
];

/// The subpage site keyed `key` (its host without `www.`), or of the host
/// `key`.
pub fn site(key: &str) -> Option<&'static SubpageSite> {
    let key = key.strip_prefix("www.").unwrap_or(key);
    SUBPAGE_SITES.iter().find(|site| site.key() == key)
}

/// Most pages the set can hold: every site's cap.
pub fn most_pages() -> usize {
    SUBPAGE_SITES.iter().map(SubpageSite::max_pages).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn sites_are_well_formed_and_listed_once() {
        let mut keys = HashSet::new();
        for site in SUBPAGE_SITES {
            assert!(!site.site.host.contains('/'), "{}", site.site.host);
            assert!((1..=10).contains(&site.site.weight), "{}", site.key());
            for root in site.index_pages() {
                assert!(root.starts_with("https://"), "{root}");
                assert!(
                    root.starts_with(&format!("https://{}/", site.site.host)),
                    "{root} is not on {}",
                    site.site.host
                );
            }
            assert!(keys.insert(site.key()), "{} is listed twice", site.key());
            // The reference set has its own sites; none is fetched twice.
            assert!(
                crate::reference::site(site.key()).is_none(),
                "{} is a reference site",
                site.key()
            );
        }
        for kind in [
            "university",
            "company",
            "government",
            "entertainment",
            "museum",
        ] {
            let kind = SubpageKind::of_name(kind).unwrap();
            assert!(SUBPAGE_SITES.iter().any(|site| site.kind == kind));
        }
    }

    #[test]
    fn the_set_stays_small() {
        // About 300 bytes a page: well under 100 MB on a node.
        assert!(most_pages() <= 450_000, "{}", most_pages());
    }

    #[test]
    fn sites_are_found_by_key_or_host() {
        let cpw = site("chessprogramming.org").unwrap();
        assert_eq!(cpw.kind, SubpageKind::University);
        assert_eq!(
            cpw.index_pages(),
            [
                "https://chessprogramming.org/",
                "https://chessprogramming.org/Special:AllPages"
            ]
        );
        assert_eq!(cpw.max_pages(), 5_000);
        assert_eq!(
            site("norvig.com").unwrap().roots(),
            ["https://www.norvig.com/", "https://norvig.com/"]
        );
        assert_eq!(site("www.nist.gov").unwrap().key(), "nist.gov");
        assert!(site("example.com").is_none());
        assert_eq!(site("norvig.com").unwrap().max_pages(), DEFAULT_MAX_PAGES);
    }
}
