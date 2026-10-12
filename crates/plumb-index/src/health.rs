//! Health authorities first for medical searches: "ibuprofen dosage",
//! "flu symptoms" and "metformin side effects" otherwise list whatever
//! homepage has the words (dosage-poudre.fr, side.co, pill shops). A short
//! list of public and clinical health sites goes above them.

use plumb_core::normalize_text;
use serde::{Deserialize, Serialize};

use crate::{Hit, WELL_KNOWN_LINK_SCORE};

/// How many health authorities go first.
const AUTHORITIES: usize = 3;

/// The reason for a source-quality decision. The legacy domain heuristic
/// is explicitly distinct from an observed prescription-free sales offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceQualityReason {
    HealthAuthority,
    GovernmentAuthority,
    DrugDomainPattern,
    UnprescribedDrugOffer,
}

/// Bounded source evidence usable by site and page callers and evaluation
/// traces. Institutional identity does not establish relevance to a query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceQualityEvidence {
    pub reason: SourceQualityReason,
    /// Owner's identity page, verified October 9, 2026, for the existing
    /// health sources. Content-pattern decisions have no owner attestation.
    pub provenance: Option<String>,
}

impl SourceQualityEvidence {
    pub fn informational_penalty(&self) -> bool {
        matches!(
            self.reason,
            SourceQualityReason::DrugDomainPattern | SourceQualityReason::UnprescribedDrugOffer
        )
    }
}

/// Evidence from existing institutional source identities or an explicit drug
/// sales offer. Unknown sources, low popularity and TLDs alone add none.
/// Pages may pass `None` for the unavailable site link score.
pub fn source_quality(
    domain: &str,
    title: Option<&str>,
    description: Option<&str>,
    link_score: Option<f32>,
) -> Option<SourceQualityEvidence> {
    let host = plumb_core::host_of(domain).unwrap_or_else(|| domain.to_ascii_lowercase());
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let provenance = match host {
        "medlineplus.gov" => Some("https://medlineplus.gov/about/general/aboutmedlineplus/"),
        "nhs.uk" => Some("https://www.nhs.uk/about-us/"),
        "mayoclinic.org" => Some("https://www.mayoclinic.org/about-mayo-clinic"),
        "irs.gov" => Some("https://www.irs.gov/about-irs"),
        "clevelandclinic.org" | "my.clevelandclinic.org" => {
            Some("https://my.clevelandclinic.org/about")
        }
        _ => None,
    };
    if let Some(provenance) = provenance {
        return Some(SourceQualityEvidence {
            reason: if host == "irs.gov" {
                SourceQualityReason::GovernmentAuthority
            } else {
                SourceQualityReason::HealthAuthority
            },
            provenance: Some(provenance.to_string()),
        });
    }
    let offer = title.into_iter().chain(description).any(|text| {
        let text = normalize_text(text);
        let words: Vec<_> = text.split_whitespace().collect();
        // An article quoting or warning about an offer is not an offer.
        let selling = words
            .first()
            .is_some_and(|word| ["buy", "order", "shop"].contains(word));
        let drug = words
            .get(1)
            .is_some_and(|word| crate::PILL_SHOP_DRUGS.contains(word))
            || words.get(1) == Some(&"generic")
                && words
                    .get(2)
                    .is_some_and(|word| crate::PILL_SHOP_DRUGS.contains(word));
        selling
            && drug
            && [
                " no prescription ",
                " without prescription ",
                " without a prescription ",
                " prescription not required ",
            ]
            .iter()
            .any(|phrase| format!(" {text} ").contains(phrase))
    });
    let reason = if offer {
        SourceQualityReason::UnprescribedDrugOffer
    } else if link_score.is_some_and(|score| crate::is_pill_shop(host, score)) {
        SourceQualityReason::DrugDomainPattern
    } else {
        return None;
    };
    Some(SourceQualityEvidence {
        reason,
        provenance: None,
    })
}

/// The same observed-offer policy for deep site pages. Exact host
/// navigation remains available; an exact informational page title does
/// not exempt a prescription-free drug offer.
pub(crate) fn page_allowed(query: &str, page: &crate::pages::Page) -> bool {
    if !page.is_site_page()
        || crate::typed_domain(query).is_some_and(|domain| {
            plumb_core::registrable_domain(&page.url).as_deref() == Some(domain.as_str())
        })
    {
        return true;
    }
    !source_quality(
        &page.url,
        Some(&page.title),
        page.description.as_deref(),
        None,
    )
    .is_some_and(|quality| quality.informational_penalty())
}

/// The health authorities for a searcher in `country`, best first, by the
/// domain the index has them under.
fn authorities(country: Option<&str>) -> &'static [&'static str] {
    match country.map(str::to_ascii_uppercase).as_deref() {
        Some("GB" | "UK") => &["www.nhs.uk", "medlineplus.gov", "mayoclinic.org"],
        _ => &["medlineplus.gov", "mayoclinic.org", "clevelandclinic.org"],
    }
}

/// Words that make a search a medical one wherever they appear.
const MEDICAL_WORDS: &[&str] = &[
    // What is asked about an illness or a drug.
    "symptom",
    "symptoms",
    "dosage",
    "dose",
    "doses",
    "overdose",
    "diagnosis",
    "contraindications",
    // Illnesses.
    "diabetes",
    "cancer",
    "migraine",
    "migraines",
    "asthma",
    "influenza",
    "flu",
    "covid",
    "pneumonia",
    "bronchitis",
    "hypertension",
    "arthritis",
    "dementia",
    "alzheimers",
    "eczema",
    "psoriasis",
    "shingles",
    "measles",
    "chickenpox",
    "strep",
    "hiv",
    "herpes",
    "tonsillitis",
    "sinusitis",
    "insomnia",
    "anemia",
    "cholesterol",
    "concussion",
    "appendicitis",
    "gout",
    "lupus",
    "fibromyalgia",
    "endometriosis",
    "uti",
    "adhd",
    // Common drugs (the pill-shop names count too, see `is_medical`).
    "ibuprofen",
    "acetaminophen",
    "paracetamol",
    "tylenol",
    "advil",
    "aspirin",
    "naproxen",
    "metformin",
    "lisinopril",
    "atorvastatin",
    "omeprazole",
    "amlodipine",
    "levothyroxine",
    "sertraline",
    "insulin",
    "benadryl",
    "melatonin",
];

/// Phrases that make a search a medical one.
const MEDICAL_PHRASES: &[&str] = &[
    "side effects",
    "blood pressure",
    "heart attack",
    "kidney stones",
    "sore throat",
    "pink eye",
    "panic attack",
    "panic attacks",
    "treatment for",
    "how to treat",
    "signs of",
];

/// Searches with these are about something else: the economy, a film, a
/// band.
const NOT_MEDICAL: &[&str] = &["great depression", "flu game", "shot of", "cancer zodiac"];

/// Whether `query` asks about an illness, a symptom or a drug.
pub(crate) fn is_medical(query: &str) -> bool {
    let query = normalize_text(query);
    let padded = format!(" {query} ");
    if NOT_MEDICAL
        .iter()
        .any(|phrase| padded.contains(&format!(" {phrase} ")))
    {
        return false;
    }
    let words: Vec<&str> = query.split(' ').collect();
    words.iter().any(|word| {
        MEDICAL_WORDS.contains(word) || crate::PILL_SHOP_DRUGS.contains(word)
    }) || MEDICAL_PHRASES
        .iter()
        .any(|phrase| padded.contains(&format!(" {phrase} ")))
        // "depression" alone or with "symptoms"/"help": not "depression era".
        || words.iter().any(|w| ["depression", "anxiety"].contains(w)) && words.len() <= 3
}

/// `hits` for a medical `query` with the health authorities first, under
/// a well-known site the query names (diabetes.org for "diabetes"):
/// those already listed move up, the others come from `site`. At most
/// `limit` hits stay.
pub(crate) fn authorities_first(
    query: &str,
    country: Option<&str>,
    hits: &mut Vec<Hit>,
    limit: usize,
    site: &dyn Fn(&str) -> Option<Hit>,
) {
    if limit == 0 || crate::typed_domain(query).is_some() || !is_medical(query) {
        return;
    }
    let mut first = Vec::new();
    for domain in authorities(country).iter().take(AUTHORITIES) {
        match hits.iter().position(|hit| hit.domain == *domain) {
            Some(at) => first.push(hits.remove(at)),
            None => {
                if let Some(mut fallback) = site(domain) {
                    // Inserting a trusted homepage is source routing,
                    // not evidence that it answers the requested task.
                    fallback.missing_words = true;
                    first.push(fallback);
                }
            }
        }
    }
    if first.is_empty() {
        return;
    }
    // Preserve useful indexed authority guidance ahead of generic
    // authority homepages. Country/source order still breaks ties.
    first.sort_by_key(|hit| std::cmp::Reverse(!hit.missing_words && hit.text_score >= 0.75));
    let at = usize::from(
        hits.first()
            .is_some_and(|hit| hit.named && hit.link_score >= WELL_KNOWN_LINK_SCORE),
    );
    hits.splice(at..at, first);
    hits.truncate(limit);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(domain: &str, named: bool, link_score: f32) -> Hit {
        Hit {
            domain: domain.to_string(),
            url: format!("https://{domain}/"),
            title: Some(domain.to_string()),
            description: None,
            score: 1.0,
            text_score: 1.0,
            link_score,
            placing_text_score: None,
            country: None,
            named,
            official: false,
            key_pages: Vec::new(),
            demand: None,
            missing_words: false,
            query_evidence: None,
        }
    }

    fn domains(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.domain.as_str()).collect()
    }

    #[test]
    fn medical_searches_are_told_apart() {
        for query in [
            "ibuprofen dosage",
            "flu symptoms",
            "metformin side effects",
            "high blood pressure treatment",
            "depression symptoms",
            "Migraine",
            "tramadol",
            "signs of a concussion",
        ] {
            assert!(is_medical(query), "{query}");
        }
        for query in [
            "great depression",
            "netflix",
            "side quest",
            "pizza in seattle",
            "depression era glass prices in ohio",
        ] {
            assert!(!is_medical(query), "{query}");
        }
    }

    #[test]
    fn source_quality_uses_identity_and_observed_offers_not_tlds() {
        let authority = source_quality(
            "https://my.clevelandclinic.org/health/drugs/",
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(authority.reason, SourceQualityReason::HealthAuthority);
        assert!(authority.provenance.is_some());
        assert!(!authority.informational_penalty());
        assert!(source_quality("medlineplus.gov.attacker.example", None, None, None).is_none());
        let tax =
            source_quality("https://www.irs.gov/retirement-plans/", None, None, None).unwrap();
        assert_eq!(tax.reason, SourceQualityReason::GovernmentAuthority);
        assert!(!tax.informational_penalty());
        assert!(source_quality("irs.gov.attacker.example", None, None, Some(0.9)).is_none());

        let offer = source_quality(
            "unknown-clinic.info",
            Some("Buy Viagra online without a prescription"),
            None,
            Some(0.9),
        )
        .unwrap();
        assert_eq!(offer.reason, SourceQualityReason::UnprescribedDrugOffer);
        assert!(offer.informational_penalty());
        for title in [
            "Viagra side effects and prescription safety",
            "Why you should never buy Viagra without a prescription",
            "Buy amoxicillin with a prescription",
            "Buy books about Viagra without a prescription", // the subject is books
        ] {
            // Only direct drug offers qualify, not a quoted phrase in an
            // informational article or an unrelated transaction.
            assert!(
                source_quality("small-source.pics", Some(title), None, Some(0.0)).is_none(),
                "{title}"
            );
        }
        assert!(source_quality("small-source.info", None, None, Some(0.0)).is_none());
    }

    #[test]
    fn typed_medical_hosts_and_jurisdiction_keep_their_intent() {
        let mut hits = vec![hit("viagra.com", true, 0.6)];
        authorities_first("viagra.com", Some("GB"), &mut hits, 5, &|domain| {
            Some(hit(domain, false, 0.6))
        });
        assert_eq!(domains(&hits), ["viagra.com"]);
        assert_eq!(authorities(Some("GB"))[0], "www.nhs.uk");
        assert!(!authorities(Some("IE")).contains(&"www.nhs.uk"));
    }

    #[test]
    fn indexed_authority_guidance_precedes_homepage_fallbacks() {
        let mut guidance = hit("mayoclinic.org", false, 0.7);
        guidance.url = "https://www.mayoclinic.org/drugs-supplements/lisinopril/".to_string();
        let mut hits = vec![guidance];
        authorities_first("lisinopril side effects", None, &mut hits, 3, &|domain| {
            Some(hit(domain, false, 0.6))
        });
        assert_eq!(domains(&hits)[0], "mayoclinic.org");
        assert!(hits[0].url.contains("lisinopril"));
        assert!(hits[1..].iter().all(|hit| hit.missing_words));
    }

    #[test]
    fn deep_page_quality_survives_placement_and_learned_reordering() {
        use crate::pages::{Page, PageHit, PlacedPage};
        let page = |url: &str, title: &str| PageHit {
            page: Page::from_reference(plumb_core::Article {
                item: Some(url.to_string()),
                title: title.to_string(),
                ..Default::default()
            })
            .unwrap(),
            score: 0.9,
            named: true,
            popularity: 0.5,
            whole: true,
            learned: None,
        };
        let offer = page(
            "https://quick-meds.info/viagra/",
            "Buy Viagra without a prescription",
        );
        let guidance = page(
            "https://medlineplus.gov/druginfo/meds/a699015.html",
            "Sildenafil side effects and dosage",
        );
        let query = "sildenafil side effects and dosage";
        let mut sites = Vec::new();
        let mut placed = crate::pages::place_pages(query, &sites, vec![offer.clone(), guidance]);
        assert_eq!(placed.len(), 1);
        assert!(placed[0]
            .hit
            .page
            .url
            .starts_with("https://medlineplus.gov/"));
        // A caller presenting a previously placed row still cannot make
        // the offer reappear through learned reordering.
        placed.push(PlacedPage {
            hit: offer.clone(),
            at: 0,
            under: None,
        });
        crate::learned::reorder(
            crate::learned::Model::builtin(),
            query,
            &mut sites,
            &mut placed,
        );
        assert_eq!(placed.len(), 1);
        assert!(page_allowed("quick-meds.info", &offer.page));
    }

    #[test]
    fn authorities_go_first_under_a_named_site() {
        let site = |domain: &str| Some(hit(domain, false, 0.6));
        let mut homepage = hit("mayoclinic.org", false, 0.68);
        homepage.text_score = 0.0;
        homepage.missing_words = true;
        let mut hits = vec![
            hit("dosage-poudre.fr", false, 0.1),
            homepage,
            hit("sildemedx.com", false, 0.05),
        ];
        authorities_first("ibuprofen dosage", Some("US"), &mut hits, 5, &site);
        assert_eq!(
            domains(&hits),
            [
                "medlineplus.gov",
                "mayoclinic.org",
                "clevelandclinic.org",
                "dosage-poudre.fr",
                "sildemedx.com"
            ]
        );

        // A well-known site the search names keeps first place.
        let mut hits = vec![
            hit("diabetes.org", true, 0.7),
            hit("diabetes-m.com", false, 0.2),
        ];
        authorities_first("diabetes", Some("GB"), &mut hits, 3, &site);
        assert_eq!(
            domains(&hits),
            ["diabetes.org", "www.nhs.uk", "medlineplus.gov"]
        );

        // Not medical, or no authority in the index: unchanged.
        let mut hits = vec![hit("pizzahut.com", true, 0.7)];
        authorities_first("pizza", None, &mut hits, 5, &site);
        assert_eq!(domains(&hits), ["pizzahut.com"]);
        authorities_first("flu symptoms", None, &mut hits, 5, &|_| None);
        assert_eq!(domains(&hits), ["pizzahut.com"]);
    }
}
