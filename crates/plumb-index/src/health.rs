//! Health authorities first for medical searches: "ibuprofen dosage",
//! "flu symptoms" and "metformin side effects" otherwise list whatever
//! homepage has the words (dosage-poudre.fr, side.co, pill shops). A short
//! list of public and clinical health sites goes above them.

use plumb_core::normalize_text;

use crate::{Hit, WELL_KNOWN_LINK_SCORE};

/// How many health authorities go first.
const AUTHORITIES: usize = 3;

/// The health authorities for a searcher in `country`, best first, by the
/// domain the index has them under.
fn authorities(country: Option<&str>) -> &'static [&'static str] {
    match country.map(str::to_ascii_uppercase).as_deref() {
        Some("GB" | "UK" | "IE") => &["www.nhs.uk", "medlineplus.gov", "mayoclinic.org"],
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
    if limit == 0 || !is_medical(query) {
        return;
    }
    let mut first = Vec::new();
    for domain in authorities(country).iter().take(AUTHORITIES) {
        match hits.iter().position(|hit| hit.domain == *domain) {
            Some(at) => first.push(hits.remove(at)),
            None => first.extend(site(domain)),
        }
    }
    if first.is_empty() {
        return;
    }
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
    fn authorities_go_first_under_a_named_site() {
        let site = |domain: &str| Some(hit(domain, false, 0.6));
        let mut hits = vec![
            hit("dosage-poudre.fr", false, 0.1),
            hit("mayoclinic.org", false, 0.68),
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
