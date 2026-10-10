//! Reference sites: the well-known sites whose inner pages answer everyday
//! searches ("foul smelling stool", "define prioritize", "irs form 1040",
//! "how to boil eggs"), and whose pages make the `reference` page set.
//!
//! Most searches want one page of some site, not a site: a symptom page, a
//! dictionary's entry, a recipe, a form, a how-to. Each site's pages are
//! those its sitemaps list under its roots, fetched like homepages
//! (robots.txt obeyed, one request at a time), and found by the words of
//! their titles and descriptions.

/// A site whose inner pages are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceSite {
    /// Its host, as its pages' addresses have it: `www.healthline.com`.
    pub host: &'static str,
    /// Where its pages are; only pages under these are taken. Empty for
    /// the whole site.
    pub roots: &'static [&'static str],
    /// Its sitemaps, when robots.txt names none or names others.
    pub sitemaps: &'static [&'static str],
    /// Explicit task/section discovery pages; also considered page candidates.
    pub index_pages: &'static [&'static str],
    /// How much its pages weigh against other sites', 1 to 10: the most
    /// used weigh most.
    pub weight: u64,
    /// Most pages fetched, when more than the default: a dictionary has a
    /// page for every word.
    pub max_pages: Option<usize>,
}

impl ReferenceSite {
    /// Where its pages are: its roots, or its whole host.
    pub fn roots(&self) -> Vec<String> {
        if self.roots.is_empty() {
            vec![format!("https://{}/", self.host)]
        } else {
            self.roots.iter().map(|root| root.to_string()).collect()
        }
    }

    /// Task indexes, falling back to the permitted roots for link discovery.
    pub fn index_pages(&self) -> Vec<String> {
        let mut pages = self.roots();
        pages.extend(self.index_pages.iter().map(|p| p.to_string()));
        pages
    }

    /// Its host without `www.`: `healthline.com`, as the site is keyed.
    pub fn key(&self) -> &'static str {
        self.host.strip_prefix("www.").unwrap_or(self.host)
    }
}

/// A site of `weight` whose pages are anywhere on `host`.
const fn whole(host: &'static str, weight: u64) -> ReferenceSite {
    ReferenceSite {
        host,
        roots: &[],
        sitemaps: &[],
        index_pages: &[],
        weight,
        max_pages: None,
    }
}

/// A site of `weight` whose pages are under `roots`.
const fn under(host: &'static str, roots: &'static [&'static str], weight: u64) -> ReferenceSite {
    ReferenceSite {
        host,
        roots,
        sitemaps: &[],
        index_pages: &[],
        weight,
        max_pages: None,
    }
}

/// A site of `weight` with up to `max_pages` pages.
const fn big(host: &'static str, weight: u64, max_pages: usize) -> ReferenceSite {
    ReferenceSite {
        host,
        roots: &[],
        sitemaps: &[],
        index_pages: &[],
        weight,
        max_pages: Some(max_pages),
    }
}

/// A bounded task batch with explicit discovery hints. Existing hosts keep
/// their permitted whole-site roots so a targeted refresh retains old tasks.
const fn tasks(
    host: &'static str,
    roots: &'static [&'static str],
    indexes: &'static [&'static str],
    weight: u64,
    cap: usize,
) -> ReferenceSite {
    ReferenceSite {
        host,
        roots,
        sitemaps: &[],
        index_pages: indexes,
        weight,
        max_pages: Some(cap),
    }
}

/// The reference sites, by what they are about, the most used of each
/// first.
pub const REFERENCE_SITES: &[ReferenceSite] = &[
    // Bounded initial Spanish/German institutional coverage. Language is
    // taken from each fetched page, not assigned to the whole host.
    ReferenceSite {
        host: "www.dnielectronico.es",
        roots: &["https://www.dnielectronico.es/PortalDNIe/"],
        sitemaps: &[],
        weight: 6,
        max_pages: Some(500),
    },
    ReferenceSite {
        host: "gesund.bund.de",
        roots: &["https://gesund.bund.de/"],
        sitemaps: &[],
        weight: 6,
        max_pages: Some(500),
    },
    // Health.
    big("www.webmd.com", 10, 10_000),
    big("www.healthline.com", 10, 10_000),
    big("www.mayoclinic.org", 10, 10_000),
    big("www.drugs.com", 9, 10_000),
    big("www.medicinenet.com", 8, 10_000),
    under("medlineplus.gov", &["https://medlineplus.gov/"], 9),
    tasks(
        "www.cdc.gov",
        &[],
        &[
            "https://www.cdc.gov/health-topics.html",
            "https://www.cdc.gov/flu/prevention/index.html",
            "https://www.cdc.gov/covid/prevention/index.html",
        ],
        9,
        5_000,
    ),
    under(
        "www.nhs.uk",
        &[
            "https://www.nhs.uk/conditions/",
            "https://www.nhs.uk/medicines/",
            "https://www.nhs.uk/live-well/",
        ],
        8,
    ),
    under(
        "my.clevelandclinic.org",
        &["https://my.clevelandclinic.org/health/"],
        8,
    ),
    whole("www.medicalnewstoday.com", 7),
    whole("www.verywellhealth.com", 7),
    whole("www.everydayhealth.com", 6),
    whole("www.emedicinehealth.com", 5),
    whole("www.rxlist.com", 6),
    whole("patient.info", 6),
    under(
        "www.health.harvard.edu",
        &["https://www.health.harvard.edu/"],
        5,
    ),
    under(
        "www.hopkinsmedicine.org",
        &["https://www.hopkinsmedicine.org/health/"],
        5,
    ),
    whole("kidshealth.org", 5),
    whole("www.cancer.org", 5),
    whole("www.heart.org", 5),
    whole("www.niddk.nih.gov", 5),
    whole("www.nhlbi.nih.gov", 5),
    whole("www.merckmanuals.com", 6),
    whole("www.babycenter.com", 6),
    whole("www.whattoexpect.com", 5),
    whole("www.livestrong.com", 5),
    whole("www.testing.com", 4),
    whole("www.verywellmind.com", 5),
    whole("www.verywellfit.com", 5),
    whole("www.petmd.com", 4),
    whole("vcahospitals.com", 4),
    whole("www.akc.org", 4),
    // Words: dictionaries and thesauruses.
    big("www.merriam-webster.com", 10, 20_000),
    big("www.dictionary.com", 9, 20_000),
    big("www.thesaurus.com", 7, 10_000),
    big("www.vocabulary.com", 6, 10_000),
    big("www.collinsdictionary.com", 6, 10_000),
    big("dictionary.cambridge.org", 7, 10_000),
    big("www.thefreedictionary.com", 6, 10_000),
    whole("www.yourdictionary.com", 4),
    whole("www.spanishdict.com", 5),
    whole("www.grammarly.com", 4),
    whole("www.grammarbook.com", 3),
    whole("owl.purdue.edu", 5),
    // Learning and reference.
    big("www.britannica.com", 9, 10_000),
    whole("www.thoughtco.com", 8),
    whole("www.history.com", 6),
    whole("www.biography.com", 6),
    whole("www.khanacademy.org", 6),
    whole("www.mathsisfun.com", 6),
    whole("www.livescience.com", 6),
    whole("www.nationalgeographic.com", 5),
    whole("www.worldatlas.com", 5),
    whole("www.infoplease.com", 4),
    whole("www.ducksters.com", 4),
    whole("www.space.com", 5),
    whole("www.nasa.gov", 5),
    whole("www.timeanddate.com", 7),
    whole("www.calculator.net", 5),
    whole("www.omnicalculator.com", 5),
    whole("www.rapidtables.com", 4),
    whole("www.unitconverters.net", 4),
    whole("www.scienceabc.com", 3),
    whole("study.com", 6),
    whole("www.sparknotes.com", 5),
    whole("www.litcharts.com", 4),
    whole("www.poetryfoundation.org", 4),
    whole("www.gutenberg.org", 3),
    // Food.
    whole("www.allrecipes.com", 9),
    whole("www.foodnetwork.com", 7),
    whole("www.simplyrecipes.com", 6),
    whole("www.seriouseats.com", 5),
    whole("www.thekitchn.com", 5),
    whole("www.tasteofhome.com", 6),
    whole("www.delish.com", 5),
    whole("www.bbcgoodfood.com", 6),
    whole("www.food.com", 5),
    whole("www.epicurious.com", 5),
    whole("www.bonappetit.com", 4),
    whole("www.thespruceeats.com", 6),
    whole("www.budgetbytes.com", 3),
    whole("www.eatingwell.com", 5),
    whole("www.myrecipes.com", 4),
    // How-to: homes, gardens, computers and phones.
    whole("www.wikihow.com", 9),
    whole("www.thespruce.com", 6),
    whole("www.bhg.com", 5),
    whole("www.hgtv.com", 4),
    whole("www.familyhandyman.com", 5),
    whole("www.bobvila.com", 4),
    whole("www.almanac.com", 5),
    whole("www.gardeningknowhow.com", 4),
    whole("www.lifewire.com", 7),
    whole("www.howtogeek.com", 6),
    whole("www.makeuseof.com", 4),
    whole("www.digitaltrends.com", 4),
    whole("www.pcmag.com", 4),
    whole("www.cnet.com", 5),
    under(
        "support.microsoft.com",
        &["https://support.microsoft.com/en-us/"],
        9,
    ),
    under(
        "support.apple.com",
        &["https://support.apple.com/en-us/"],
        9,
    ),
    under("support.google.com", &["https://support.google.com/"], 8),
    under("helpx.adobe.com", &["https://helpx.adobe.com/"], 5),
    whole("www.w3schools.com", 6),
    whole("www.geeksforgeeks.org", 5),
    whole("www.tutorialspoint.com", 4),
    // Money and work.
    whole("www.investopedia.com", 9),
    whole("www.thebalancemoney.com", 6),
    whole("www.nerdwallet.com", 6),
    whole("www.bankrate.com", 6),
    whole("www.fool.com", 4),
    whole("www.consumerfinance.gov", 5),
    whole("www.indeed.com", 4),
    whole("www.thebalancecareers.com", 3),
    // Government.
    big("www.gov.uk", 10, 10_000),
    tasks(
        "www.irs.gov",
        &[],
        &[
            "https://www.irs.gov/forms-instructions",
            "https://www.irs.gov/retirement-plans",
            "https://www.irs.gov/payments",
        ],
        10,
        5_000,
    ),
    tasks(
        "www.ssa.gov",
        &[],
        &[
            "https://www.ssa.gov/benefits",
            "https://www.ssa.gov/number-card",
            "https://www.ssa.gov/medicare",
        ],
        8,
        5_000,
    ),
    whole("www.usa.gov", 7),
    whole("www.uscis.gov", 6),
    whole("travel.state.gov", 6),
    whole("www.va.gov", 6),
    whole("www.medicare.gov", 6),
    whole("www.healthcare.gov", 5),
    tasks(
        "studentaid.gov",
        &[],
        &[
            "https://studentaid.gov/understand-aid",
            "https://studentaid.gov/manage-loans",
            "https://studentaid.gov/h/apply-for-aid/fafsa",
        ],
        5,
        5_000,
    ),
    tasks(
        "www.treasurydirect.gov",
        &[
            "https://www.treasurydirect.gov/savings-bonds/",
            "https://www.treasurydirect.gov/marketable-securities/",
        ],
        &["https://www.treasurydirect.gov/savings-bonds/"],
        7,
        1_000,
    ),
    tasks(
        "tools.usps.com",
        &["https://tools.usps.com/go/"],
        &[
            "https://tools.usps.com/go/TrackConfirmAction_input",
            "https://tools.usps.com/go/ZipLookupAction_input",
            "https://tools.usps.com/go/POLocatorAction_input",
        ],
        6,
        100,
    ),
    tasks(
        "tfl.gov.uk",
        &[
            "https://tfl.gov.uk/plan-a-journey/",
            "https://tfl.gov.uk/fares/",
            "https://tfl.gov.uk/status-updates/",
            "https://tfl.gov.uk/travel-information/",
        ],
        &[
            "https://tfl.gov.uk/plan-a-journey/",
            "https://tfl.gov.uk/fares/",
            "https://tfl.gov.uk/status-updates/",
        ],
        7,
        1_000,
    ),
    whole("www.fda.gov", 5),
    whole("www.epa.gov", 4),
    whole("www.bls.gov", 5),
    whole("www.nps.gov", 6),
    whole("www.dmv.org", 5),
    whole("faq.usps.com", 5),
    under("www.canada.ca", &["https://www.canada.ca/en/"], 5),
    whole("www.australia.gov.au", 3),
    // Law.
    whole("www.law.cornell.edu", 6),
    whole("www.findlaw.com", 5),
    whole("www.nolo.com", 5),
    whole("www.justia.com", 4),
    // Cars and travel.
    whole("www.edmunds.com", 5),
    whole("www.kbb.com", 5),
    whole("www.caranddriver.com", 4),
    whole("www.lonelyplanet.com", 4),
    whole("www.tripsavvy.com", 4),
    // Parents and pets.
    whole("www.parents.com", 4),
    whole("www.thebump.com", 3),
];

/// The site of a page at `url`: the one whose host it is on.
pub fn site_of_url(url: &str) -> Option<&'static ReferenceSite> {
    let host = crate::host_of(url)?;
    REFERENCE_SITES.iter().find(|site| site.host == host)
}

/// The site keyed `key` (its host without `www.`).
pub fn site(key: &str) -> Option<&'static ReferenceSite> {
    REFERENCE_SITES.iter().find(|site| site.key() == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites_are_listed_once_with_good_roots() {
        let mut seen = std::collections::HashSet::new();
        for site in REFERENCE_SITES {
            assert!(seen.insert(site.host), "{} twice", site.host);
            assert!((1..=10).contains(&site.weight), "{}", site.host);
            for root in site.roots() {
                assert!(root.starts_with("https://"), "{root}");
                assert!(root.ends_with('/'), "{root}");
            }
            for index in site.index_pages {
                assert!(index.starts_with("https://"), "{index}");
                assert_eq!(crate::host_of(index).as_deref(), Some(site.host));
            }
        }
    }

    #[test]
    fn finds_the_site_of_a_page() {
        let site = site_of_url("https://www.healthline.com/health/foul-smelling-stool").unwrap();
        assert_eq!(site.key(), "healthline.com");
        assert!(site_of_url("https://example.com/").is_none());
        assert_eq!(super::site("merriam-webster.com").unwrap().weight, 10);
    }
}
