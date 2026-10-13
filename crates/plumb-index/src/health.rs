//! Health authorities first for medical searches: "ibuprofen dosage",
//! "flu symptoms" and "metformin side effects" otherwise list whatever
//! homepage has the words (dosage-poudre.fr, side.co, pill shops). A short
//! list of public and clinical health sites goes above them.

use plumb_core::normalize_text;
use serde::{Deserialize, Serialize};

use crate::pages::{LearnedPlace, PlacedPage};
use crate::{Hit, WELL_KNOWN_LINK_SCORE};

/// How many health authorities go first.
const AUTHORITIES: usize = 3;

/// What a drug offer says, after [`normalize_text`], to sell without a
/// prescription: "Buy Generic Viagra ✓ ED Drugs Online No Rx".
const NO_PRESCRIPTION: &[&str] = &[
    "no prescription",
    "without prescription",
    "without a prescription",
    "prescription not required",
    "no rx",
];

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
        // A part of its own: "Online-drug-store | Big Discounts | Without a
        // Prescription | Free Shipping".
        if text
            .split(['|', '-', '–', '—', ':'])
            .map(normalize_text)
            .any(|part| {
                let part = part
                    .strip_suffix(" required")
                    .or_else(|| part.strip_suffix(" needed"))
                    .unwrap_or(&part);
                NO_PRESCRIPTION.contains(&part)
            })
        {
            return true;
        }
        let text = normalize_text(text);
        let words: Vec<_> = text.split_whitespace().collect();
        // An article quoting or warning about an offer is not an offer.
        let selling = words
            .first()
            .is_some_and(|word| ["buy", "order", "shop"].contains(word));
        let drug = words
            .get(1)
            .is_some_and(|word| PILL_SHOP_DRUGS.contains(word))
            || words.get(1) == Some(&"generic")
                && words
                    .get(2)
                    .is_some_and(|word| PILL_SHOP_DRUGS.contains(word));
        selling
            && drug
            && NO_PRESCRIPTION
                .iter()
                .any(|phrase| format!(" {text} ").contains(&format!(" {phrase} ")))
    });
    let reason = if offer {
        SourceQualityReason::UnprescribedDrugOffer
    } else if link_score.is_some_and(|score| is_pill_shop(host, score)) {
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
        || crate::names::typed_domain(query).is_some_and(|domain| {
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
    "tuberculosis",
    "sclerosis",
    "glaucoma",
    "cataracts",
    "osteoporosis",
    "cirrhosis",
    "fibrosis",
    "scoliosis",
    "epilepsy",
    "autism",
    "parkinsons",
    "tinnitus",
    "apnea",
    "sepsis",
    "leukemia",
    "anaemia",
    "malaria",
    "cholera",
    "dengue",
    "rabies",
    "tetanus",
    "mumps",
    "rubella",
    "norovirus",
    "rsv",
    "mpox",
    "acne",
    "rosacea",
    "hemorrhoids",
    // Symptoms.
    "headache",
    "headaches",
    "nausea",
    "dizziness",
    "vomiting",
    "diarrhea",
    "diarrhoea",
    "constipation",
    "heartburn",
    "indigestion",
    "toothache",
    "earache",
    "nosebleed",
    "nosebleeds",
    "sunburn",
    "sciatica",
    "palpitations",
    "bloating",
    "dandruff",
    "snoring",
    "ingrown",
    "wart",
    "warts",
    "cyst",
    "cysts",
    "hernia",
    "bunion",
    "bunions",
    "lice",
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
    "ozempic",
    "wegovy",
    "semaglutide",
    "mounjaro",
    "tirzepatide",
    "warfarin",
    "eliquis",
    "statin",
    "statins",
    "losartan",
    "cetirizine",
    "zyrtec",
    "loratadine",
    "claritin",
    "fluoxetine",
    "escitalopram",
    "trazodone",
    "bupropion",
    "adderall",
    "naloxone",
    "fentanyl",
    "heroin",
    "epipen",
    "antibiotic",
    "antibiotics",
];

/// Words that say a longer search is about an illness, whatever the
/// illness is called: "lyme disease", "bipolar disorder", "down
/// syndrome", "vitamin d deficiency", or a symptom of one: "dry cough",
/// "rash on arms". Alone they may be a film or a band.
const ILLNESS_WORDS: &[&str] = &[
    "disease",
    "diseases",
    "disorder",
    "disorders",
    "syndrome",
    "infection",
    "infections",
    "deficiency",
    "allergy",
    "allergies",
    "vaccine",
    "vaccines",
    "vaccination",
    "cough",
    "coughing",
    "rash",
];

/// Phrases that make a search a medical one.
const MEDICAL_PHRASES: &[&str] = &[
    "side effects",
    "blood pressure",
    "heart attack",
    "kidney stone",
    "kidney stones",
    "pink eye",
    "panic attack",
    "panic attacks",
    "treatment for",
    "how to treat",
    "signs of",
    "acid reflux",
    "food poisoning",
    "back pain",
    "pain relief",
    "upset stomach",
    "frozen shoulder",
    "shin splints",
    "carpal tunnel",
    "cold sore",
    "cold sores",
    "canker sore",
    "canker sores",
    "black eye",
    "head injury",
    "hair loss",
    "shortness of breath",
    "blood in urine",
    "blood in stool",
    "common cold",
    "head cold",
    // Not "dry" alone: "dry rub ribs".
    "dry skin",
    "dry eye",
    "dry eyes",
    "dry mouth",
    "dry scalp",
    "blood clot",
    // A fever, not a film or a drink: "saturday night fever", "fever-tree".
    "fever in",
    "high fever",
    "low grade fever",
    "hay fever",
    "scarlet fever",
    "yellow fever",
];

/// Searches with these are about something else: the economy, a film, a
/// band.
const NOT_MEDICAL: &[&str] = &[
    "great depression",
    "flu game",
    "shot of",
    "cancer zodiac",
    "china syndrome",
    "rash guard",
    "rash decision",
    "sight for sore eyes",
    "stiff upper lip",
];

/// What hurts or is wrong in a part of the body. Beside a body part it
/// makes a search medical: "knee pain", "swollen ankles", "numbness on
/// top of foot". Alone or about a thing it doesn't: "broken arrow",
/// "pulled pork shoulder".
const COMPLAINTS: &[&str] = &[
    "pain",
    "painful",
    "ache",
    "aches",
    "aching",
    "sore",
    "swollen",
    "swelling",
    "itch",
    "itchy",
    "itching",
    "numb",
    "numbness",
    "tingling",
    "cramp",
    "cramps",
    "cramping",
    "spasm",
    "spasms",
    "twitching",
    "stiff",
    "stiffness",
    "tightness",
    "weakness",
    "broken",
    "fracture",
    "fractured",
    "cracked",
    "sprain",
    "sprained",
    "torn",
    "pulled",
    "dislocated",
    "bruise",
    "bruised",
    "bruising",
    "injury",
    "injuries",
    "injured",
    "infected",
    "bleeding",
    "burning",
    "ringing",
    "lump",
    "lumps",
    "bump",
    "bumps",
    "blister",
    "blisters",
    "chapped",
    "runny",
    "stuffy",
    "blocked",
    "blurry",
    "blurred",
];

/// Parts of the body a complaint is felt in. Not "head", "back" or
/// "heart", which go with too much else: "cracked head gasket", "pulled
/// back hair", "broken heart". "Back pain" is a phrase.
const BODY_PARTS: &[&str] = &[
    "ear",
    "ears",
    "eye",
    "eyes",
    "eyelid",
    "vision",
    "nose",
    "sinus",
    "sinuses",
    "mouth",
    "lip",
    "lips",
    "tongue",
    "tooth",
    "teeth",
    "gums",
    "jaw",
    "throat",
    "neck",
    "lymph",
    "gland",
    "glands",
    "shoulder",
    "shoulders",
    "arm",
    "arms",
    "armpit",
    "elbow",
    "elbows",
    "wrist",
    "wrists",
    "hand",
    "hands",
    "finger",
    "fingers",
    "thumb",
    "knuckles",
    "chest",
    "breast",
    "breasts",
    "rib",
    "ribs",
    "spine",
    "tailbone",
    "stomach",
    "abdomen",
    "abdominal",
    "belly",
    "liver",
    "kidney",
    "kidneys",
    "bladder",
    "pelvis",
    "pelvic",
    "groin",
    "testicle",
    "testicles",
    "hip",
    "hips",
    "leg",
    "legs",
    "thigh",
    "thighs",
    "hamstring",
    "knee",
    "knees",
    "acl",
    "meniscus",
    "shin",
    "shins",
    "calf",
    "calves",
    "ankle",
    "ankles",
    "achilles",
    "foot",
    "feet",
    "heel",
    "heels",
    "toe",
    "toes",
    "toenail",
    "skin",
    "scalp",
    "joint",
    "joints",
    "muscle",
    "muscles",
    "tendon",
    "ligament",
];

/// Words that say where in a body part: "pain in left arm", "numbness on
/// top of foot". Only these may come between a complaint and its body
/// part, so "pulled pork shoulder" and "dry rub ribs" are about food.
const PLACE_WORDS: &[&str] = &[
    "in", "on", "of", "the", "my", "a", "an", "both", "left", "right", "top", "bottom", "side",
    "front", "inner", "outer", "inside", "outside", "upper", "lower", "under", "behind", "around",
    "between",
];

/// Whether `words` say something is wrong in a part of the body: a
/// complaint with a body part next to it, or with only place words
/// between them.
fn complaint_in_body(words: &[&str]) -> bool {
    let not_place = |word: &&&str| !PLACE_WORDS.contains(*word);
    let body = |word: Option<&&str>| word.is_some_and(|word| BODY_PARTS.contains(word));
    words.iter().enumerate().any(|(i, word)| {
        COMPLAINTS.contains(word)
            && (body(words[..i].iter().rev().find(not_place))
                || body(words[i + 1..].iter().find(not_place)))
    })
}

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
        MEDICAL_WORDS.contains(word)
            || PILL_SHOP_DRUGS.contains(word)
            || words.len() > 1 && ILLNESS_WORDS.contains(word)
            // An inflammation: hepatitis, meningitis, colitis.
            || word.len() > 5 && word.ends_with("itis")
    }) || MEDICAL_PHRASES
        .iter()
        .any(|phrase| padded.contains(&format!(" {phrase} ")))
        // "depression" alone or with "symptoms"/"help": not "depression era".
        || words.iter().any(|w| ["depression", "anxiety"].contains(w)) && words.len() <= 3
        || complaint_in_body(&words)
}

/// Health sources searchers ask for by name, by the domain the index has
/// them under: "how long is the flu contagious CDC", "lisinopril
/// MedlinePlus". Lowercase names match the folded query; a name with
/// capitals matches only as written, so "who" in a question is not WHO.
const SOURCES: &[(&str, &[&str])] = &[
    ("cdc.gov", &["cdc"]),
    ("www.nhs.uk", &["nhs"]),
    ("medlineplus.gov", &["medlineplus", "medline plus"]),
    ("mayoclinic.org", &["mayoclinic", "mayo clinic"]),
    (
        "clevelandclinic.org",
        &["clevelandclinic", "cleveland clinic"],
    ),
    ("who.int", &["WHO", "world health organization"]),
    ("webmd.com", &["webmd"]),
    ("healthline.com", &["healthline"]),
    ("healthychildren.org", &["healthychildren"]),
    ("nih.gov", &["NIH"]),
    ("fda.gov", &["FDA"]),
    ("ecdc.europa.eu", &["ecdc"]),
];

/// The sites `query` asks for as its source, by domain: a health source
/// it names ([`SOURCES`]) or an address typed in it ("acetaminophen dose
/// healthychildren.org", "cetirizine (site:drugs.com)", which is not an
/// operator). Doses and abbreviations ("0.5mg", "e.g.") are
/// not addresses. A typed address keeps its own host unless it only adds
/// "www.": ecdc.europa.eu is not europa.eu, whose homepage is the EU's.
fn named_sources(query: &str) -> Vec<String> {
    let folded = format!(" {} ", normalize_text(query));
    let words: Vec<&str> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let mut named: Vec<String> = SOURCES
        .iter()
        .filter(|(_, names)| {
            names.iter().any(|name| {
                if name.chars().any(|c| c.is_ascii_uppercase()) {
                    words.contains(name)
                } else {
                    folded.contains(&format!(" {name} "))
                }
            })
        })
        .map(|(domain, _)| domain.to_string())
        .collect();
    for word in query.split_whitespace() {
        let word = word.trim_matches(|c: char| !c.is_alphanumeric());
        let word = word.strip_prefix("site:").unwrap_or(word);
        let ends_in_letters = word
            .rsplit_once('.')
            .is_some_and(|(_, end)| end.len() > 1 && end.chars().all(|c| c.is_ascii_alphabetic()));
        if let Some(host) = plumb_core::host_of(word).filter(|_| ends_in_letters) {
            let domain = plumb_core::registrable_domain(&host)
                .filter(|domain| same_site(domain, &host))
                .unwrap_or(host);
            if !named.iter().any(|known| same_site(known, &domain)) {
                named.push(domain);
            }
        }
    }
    named
}

/// Whether two index domains are one site, with or without `www.`.
fn same_site(a: &str, b: &str) -> bool {
    a.trim_start_matches("www.") == b.trim_start_matches("www.")
}

/// Whether a word of `query` is the name of `hit`'s site and neither
/// medical nor `common` (in many sites' titles): "medscape" for
/// medscape.com, "hellopharmacist" for hellopharmacist.com. Such a search
/// asks that site, not the authorities. An illness, a drug or a part of
/// the body that is also a site's name is what the search is about:
/// diabetes.org for "diabetes symptoms", hands.net for "tingling in
/// hands". And first.org is not asked for by "first dose".
fn names_site(query: &str, hit: &Hit, common: &dyn Fn(&str) -> bool) -> bool {
    let Some(domain) = plumb_core::registrable_domain(&hit.domain) else {
        return false;
    };
    let label = domain.split('.').next().unwrap_or_default();
    normalize_text(query)
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word == label)
        && !is_medical(label)
        && !BODY_PARTS.contains(&label)
        && !common(label)
}

/// `hits` for a medical `query` with the health authorities first, under
/// a well-known site the query names (diabetes.org for "diabetes"):
/// those already listed move up, the others come from `site`. A source
/// the query asks for ([`named_sources`]) goes above them, from `site`
/// too when it isn't listed, and so does a listed site it names by a word
/// that is neither medical nor `common` ([`names_site`]). At most `limit`
/// hits stay.
pub(crate) fn authorities_first(
    query: &str,
    country: Option<&str>,
    hits: &mut Vec<Hit>,
    limit: usize,
    site: &dyn Fn(&str) -> Option<Hit>,
    common: &dyn Fn(&str) -> bool,
) {
    if limit == 0 || crate::names::typed_domain(query).is_some() || !is_medical(query) {
        return;
    }
    let mut sources = Vec::new();
    for domain in named_sources(query) {
        match hits.iter().position(|hit| same_site(&hit.domain, &domain)) {
            Some(at) => sources.push(hits.remove(at)),
            None => {
                if let Some(mut fallback) = site(&domain) {
                    fallback.missing_words = true;
                    sources.push(fallback);
                }
            }
        }
    }
    let (named, rest) = std::mem::take(hits)
        .into_iter()
        .partition::<Vec<Hit>, _>(|hit| names_site(query, hit, common));
    *hits = rest;
    sources.extend(named);
    let mut first = Vec::new();
    for domain in authorities(country).iter().take(AUTHORITIES) {
        if sources
            .iter()
            .any(|hit: &Hit| same_site(&hit.domain, domain))
        {
            continue;
        }
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
    if first.is_empty() && sources.is_empty() {
        return;
    }
    // Preserve useful indexed authority guidance ahead of generic
    // authority homepages. Country/source order still breaks ties.
    first.sort_by_key(|hit| std::cmp::Reverse(!hit.missing_words && hit.text_score >= 0.75));
    let at = usize::from(
        sources.is_empty()
            && hits
                .first()
                .is_some_and(|hit| hit.named && hit.link_score >= WELL_KNOWN_LINK_SCORE),
    );
    hits.splice(at..at, sources.into_iter().chain(first));
    hits.truncate(limit);
}

/// The sources a medical `query` asks for ([`named_sources`]) and the
/// sites it names by a word neither medical nor `common` ([`names_site`])
/// first among the sites again, once the learned ranking has had its say:
/// it lists a homepage [`authorities_first`] put in under the authorities
/// ("NHS paracetamol for children dose" had nhs.uk fourth). Pages listed on
/// their own keep their places, and the moved sites trade scores so that
/// scores still go down the list, as [`crate::learned::reorder`] leaves
/// them.
pub fn sources_lead(
    query: &str,
    hits: &mut Vec<Hit>,
    placed: &mut [PlacedPage],
    common: &dyn Fn(&str) -> bool,
) {
    if plumb_core::Operators::parse(query).any()
        || crate::names::typed_domain(query).is_some()
        || !is_medical(query)
    {
        return;
    }
    let mut lead: Vec<usize> = Vec::new();
    for domain in named_sources(query) {
        if let Some(at) = hits.iter().position(|hit| same_site(&hit.domain, &domain)) {
            if !lead.contains(&at) {
                lead.push(at);
            }
        }
    }
    for (at, hit) in hits.iter().enumerate() {
        if !lead.contains(&at) && names_site(query, hit, common) {
            lead.push(at);
        }
    }
    let mut new_hits: Vec<Hit> = lead.iter().map(|&at| hits[at].clone()).collect();
    new_hits.extend(
        hits.iter()
            .enumerate()
            .filter(|(at, _)| !lead.contains(at))
            .map(|(_, hit)| hit.clone()),
    );
    let Some(moved) = new_hits
        .iter()
        .zip(hits.iter())
        .rposition(|(new, old)| new.domain != old.domain)
        .map(|last| last + 1)
    else {
        return;
    };
    let mut scores: Vec<f32> = new_hits[..moved].iter().map(|hit| hit.score).collect();
    scores.sort_by(|a, b| b.total_cmp(a));
    for (hit, score) in new_hits.iter_mut().zip(scores) {
        hit.score = score;
    }
    *hits = new_hits;
    for page in placed.iter_mut().filter(|page| page.under.is_none()) {
        if matches!(page.hit.learned, Some(LearnedPlace::Before(_))) {
            if let Some(site) = hits.get(page.at) {
                page.hit.learned = Some(LearnedPlace::Before(site.domain.clone()));
            }
        }
    }
}

/// Drugs that spam pill shops put in their names.
const PILL_SHOP_DRUGS: &[&str] = &[
    "viagra",
    "cialis",
    "levitra",
    "kamagra",
    "sildenafil",
    "tadalafil",
    "vardenafil",
    "vidalista",
    "fildena",
    "cenforce",
    "celebrex",
    "ciprofloxacin",
    "flibanserin",
    "tramadol",
    "xanax",
    "alprazolam",
    "valium",
    "diazepam",
    "oxycodone",
    "hydrocodone",
    "modafinil",
    "armodafinil",
    "ivermectin",
    "hydroxychloroquine",
    "propecia",
    "finasteride",
    "clomid",
    "accutane",
    "isotretinoin",
    "priligy",
    "dapoxetine",
    "zithromax",
    "azithromycin",
    "amoxicillin",
    "doxycycline",
    "lasix",
    "furosemide",
    "prednisone",
    "neurontin",
    "gabapentin",
    "lyrica",
    "pregabalin",
    "synthroid",
    "lexapro",
    "zoloft",
    "prozac",
];

/// Whether `domain` is a little-known site named after a drug that spam
/// pill shops sell (celebrex365o24.com, vidalista.pics): such a name is
/// nearly always a shop, not a place to learn about the drug.
fn is_pill_shop(domain: &str, link_score: f32) -> bool {
    if link_score >= WELL_KNOWN_LINK_SCORE {
        return false;
    }
    let label = domain.split('.').next().unwrap_or("");
    PILL_SHOP_DRUGS.iter().any(|drug| label.contains(drug))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn hit(domain: &str, named: bool, link_score: f32) -> Hit {
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
            "lyme disease",
            "bipolar disorder",
            "down syndrome",
            "vitamin d deficiency",
            "hepatitis b",
            "multiple sclerosis",
            "tuberculosis",
            "parkinson's disease",
            "sleep apnea",
            "acid reflux",
            "ozempic",
            "bridging fibrosis",
            "what does heroin do",
            "headache behind eyes",
            "nausea after eating",
            "dizziness when standing up",
            "diarrhea remedies",
            "heartburn",
            "dry cough",
            "rash on arms",
            "fever in toddlers",
            "knee pain",
            "swollen ankles",
            "broken toe",
            "ringing in ears",
            "numbness on top of foot",
            "pain in left arm",
            "pain with ovarian cyst",
            "sunburn relief",
            "dry eyes",
            "blood in urine",
            "sore throat",
            "common cold 7 to 10 days",
        ] {
            assert!(is_medical(query), "{query}");
        }
        for query in [
            "great depression",
            "netflix",
            "side quest",
            "pizza in seattle",
            "depression era glass prices in ohio",
            "disorder",
            "the china syndrome",
            "academia",
            "mitosis vs meiosis",
            "rash",
            "rash guard",
            "saturday night fever",
            "fever-tree tonic",
            "broken arrow",
            "broken heart",
            "growing pains",
            "pulled pork shoulder",
            "dry rub ribs",
            "cracked head gasket",
            "quick dry nail polish",
            "a sight for sore eyes",
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
        for (title, description) in [
            ("Buy Generic Viagra [$0,99] ✓ ED Drugs Online No Rx", None),
            (
                "Online-drug-store | Big Discounts | Save up to 90% | Without a Prescription",
                None,
            ),
            (
                "Certified Canadian Pharmacies Online - No Prescription Required",
                Some("Explore our medications."),
            ),
        ] {
            let offer = source_quality("small-source.pics", Some(title), description, Some(0.2));
            assert_eq!(
                offer.map(|offer| offer.reason),
                Some(SourceQualityReason::UnprescribedDrugOffer),
                "{title}"
            );
        }
        for title in [
            "Viagra side effects and prescription safety",
            "Why you should never buy Viagra without a prescription",
            "Buy amoxicillin with a prescription",
            "Buy books about Viagra without a prescription", // the subject is books
            "Is no prescription needed for ibuprofen?",
            "Pocketpills | Telehealth Online Pharmacy | Free Prescription Delivery",
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
        authorities_first(
            "viagra.com",
            Some("GB"),
            &mut hits,
            5,
            &|domain| Some(hit(domain, false, 0.6)),
            &|_| true,
        );
        assert_eq!(domains(&hits), ["viagra.com"]);
        assert_eq!(authorities(Some("GB"))[0], "www.nhs.uk");
        assert!(!authorities(Some("IE")).contains(&"www.nhs.uk"));
    }

    #[test]
    fn indexed_authority_guidance_precedes_homepage_fallbacks() {
        let mut guidance = hit("mayoclinic.org", false, 0.7);
        guidance.url = "https://www.mayoclinic.org/drugs-supplements/lisinopril/".to_string();
        let mut hits = vec![guidance];
        authorities_first(
            "lisinopril side effects",
            None,
            &mut hits,
            3,
            &|domain| Some(hit(domain, false, 0.6)),
            &|_| true,
        );
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
        authorities_first("ibuprofen dosage", Some("US"), &mut hits, 5, &site, &|_| {
            true
        });
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
        authorities_first("diabetes", Some("GB"), &mut hits, 3, &site, &|_| true);
        assert_eq!(
            domains(&hits),
            ["diabetes.org", "www.nhs.uk", "medlineplus.gov"]
        );

        // A source the search asks for goes first, with the authorities
        // under it, and comes from the index when it isn't listed.
        for query in [
            "how long is the flu contagious CDC",
            "cdc.gov how flu spreads contagious period",
            "flu www.cdc.gov/flu",
            "flu facts (cdc.gov)",
        ] {
            for listed in [true, false] {
                let mut hits = vec![hit("flu.example", false, 0.3)];
                if listed {
                    hits.push(hit("cdc.gov", false, 0.8));
                }
                authorities_first(query, None, &mut hits, 5, &site, &|_| true);
                assert_eq!(
                    domains(&hits),
                    [
                        "cdc.gov",
                        "medlineplus.gov",
                        "mayoclinic.org",
                        "clevelandclinic.org",
                        "flu.example"
                    ],
                    "{query}"
                );
            }
        }
        for (query, source) in [
            ("is it a cold the flu or COVID-19 NHS", "www.nhs.uk"),
            ("lisinopril, site: nhs.uk", "www.nhs.uk"),
            ("mayo clinic plantar fasciitis symptoms", "mayoclinic.org"),
            ("lisinopril MedlinePlus drug information", "medlineplus.gov"),
            (
                "acetaminophen fever pain children healthychildren.org",
                "healthychildren.org",
            ),
            ("WHO diabetes fact sheet", "who.int"),
            ("webmd lisinopril ibuprofen interaction", "webmd.com"),
            ("lisinopril ibuprofen interaction drugs.com", "drugs.com"),
        ] {
            let mut hits = vec![
                hit("lisinoprilgo7.com", false, 0.1),
                hit(source, false, 0.6),
            ];
            authorities_first(query, Some("US"), &mut hits, 5, &|_| None, &|_| true);
            let listed = domains(&hits);
            assert_eq!(listed[0], source, "{query}");
            assert_eq!(listed.last(), Some(&"lisinoprilgo7.com"), "{query}");
        }
        // A source asked for that the index doesn't have: as before.
        let mut hits = vec![hit("pir.org", false, 0.3)];
        authorities_first(
            "acetaminophen dose healthychildren.org",
            None,
            &mut hits,
            4,
            &|domain| (domain != "healthychildren.org").then(|| hit(domain, false, 0.6)),
            &|_| true,
        );
        assert_eq!(
            domains(&hits),
            [
                "medlineplus.gov",
                "mayoclinic.org",
                "clevelandclinic.org",
                "pir.org"
            ]
        );
        // A typed address is its own site, not the larger one it is part
        // of (europa.eu's homepage is the EU's), but "www." changes nothing.
        assert_eq!(
            named_sources("ecdc.europa.eu seasonal influenza"),
            ["ecdc.europa.eu"]
        );
        assert_eq!(named_sources("lisinopril www.drugs.com"), ["drugs.com"]);
        let mut hits = vec![hit("flu.example", false, 0.3)];
        authorities_first(
            "ecdc.europa.eu seasonal influenza",
            None,
            &mut hits,
            4,
            &|domain| (domain != "ecdc.europa.eu").then(|| hit(domain, false, 0.6)),
            &|_| true,
        );
        assert_eq!(
            domains(&hits),
            [
                "medlineplus.gov",
                "mayoclinic.org",
                "clevelandclinic.org",
                "flu.example"
            ]
        );
        // Words that are some site's address name ask for no source,
        // nor does "who" in a question, a dose or an abbreviation.
        for query in [
            "diabetes symptoms",
            "how to tell if it's a cold or COVID-19 loss of smell",
            "measles vaccine coverage global rate first dose",
            "who should take metformin",
            "melatonin 0.5mg dose",
            "ibuprofen 2.5ml dosage",
            "flu symptoms, e.g. fever",
        ] {
            assert!(named_sources(query).is_empty(), "{query}");
            let mut hits = vec![
                hit("covid19.com.ua", false, 0.7),
                hit("first.org", false, 0.7),
                hit("who.int", false, 0.7),
            ];
            authorities_first(query, None, &mut hits, 3, &site, &|_| true);
            assert_eq!(
                domains(&hits),
                ["medlineplus.gov", "mayoclinic.org", "clevelandclinic.org"],
                "{query}"
            );
        }

        // Not medical, or no authority in the index: unchanged.
        let mut hits = vec![hit("pizzahut.com", true, 0.7)];
        authorities_first("pizza", None, &mut hits, 5, &site, &|_| true);
        assert_eq!(domains(&hits), ["pizzahut.com"]);
        authorities_first("flu symptoms", None, &mut hits, 5, &|_| None, &|_| true);
        assert_eq!(domains(&hits), ["pizzahut.com"]);
    }
    #[test]
    fn a_site_the_search_names_goes_above_the_authorities() {
        let site = |domain: &str| Some(hit(domain, false, 0.6));
        let common = |word: &str| ["first", "health"].contains(&word);
        let authorities = ["medlineplus.gov", "mayoclinic.org", "clevelandclinic.org"];
        // An address after "site:" that is not an operator, in brackets.
        assert_eq!(
            named_sources("mucinex zyrtec (site:drugs.com)"),
            ["drugs.com"]
        );
        for (query, listed, named) in [
            (
                "cetirizine (site:drugs.com)",
                vec!["cetirizine.example", "drugs.com"],
                vec!["drugs.com"],
            ),
            (
                "zyrtec mucinex hellopharmacist",
                vec!["hellopharmacist.com", "www.mucinex.com", "novopharma.tn"],
                vec!["hellopharmacist.com", "www.mucinex.com"],
            ),
            (
                "cetirizine medscape interactions",
                vec!["covid19-druginteractions.org", "www.medscape.com"],
                vec!["www.medscape.com"],
            ),
            (
                "can you take zyrtec and mucinex together",
                vec!["www.mucinex.com", "www.csaci.ca"],
                vec!["www.mucinex.com"],
            ),
            // Named by the illness the search is about, or by a word many
            // sites have: below the authorities.
            (
                "diabetes symptoms",
                vec!["diabetes.org", "diabetes.example"],
                vec![],
            ),
            (
                "measles vaccine first dose",
                vec!["first.org", "measles.example"],
                vec![],
            ),
            (
                "tingling in hands",
                vec!["hands.net", "hands.example"],
                vec![],
            ),
        ] {
            let mut hits = listed
                .iter()
                .map(|domain| hit(domain, false, 0.3))
                .collect();
            authorities_first(query, None, &mut hits, 8, &site, &common);
            let found = domains(&hits);
            assert_eq!(found[..named.len()], named, "{query}");
            assert_eq!(found[named.len()..named.len() + 3], authorities, "{query}");
        }
        // And again once the learned ranking has had its say.
        let mut hits = ["medlineplus.gov", "first.org", "hellopharmacist.com"]
            .map(|domain| hit(domain, false, 0.3))
            .to_vec();
        sources_lead(
            "zyrtec hellopharmacist first dose",
            &mut hits,
            &mut [],
            &common,
        );
        assert_eq!(
            domains(&hits),
            ["hellopharmacist.com", "medlineplus.gov", "first.org"]
        );
    }

    #[test]
    fn named_sources_lead_again_after_the_learned_order() {
        use crate::pages::{Page, PageHit};
        let ranked = |domain: &str, score: f32| {
            let mut hit = hit(domain, false, 0.6);
            hit.score = score;
            hit
        };
        let listed = || {
            vec![
                ranked("clevelandclinic.org", 0.9),
                ranked("medlineplus.gov", 0.8),
                ranked("mayoclinic.org", 0.7),
                ranked("www.nhs.uk", 0.6),
                ranked("pedi-stat.com", 0.5),
            ]
        };
        let page = PlacedPage {
            hit: PageHit {
                page: Page::from_reference(plumb_core::Article {
                    item: Some("https://www.verywellhealth.com/paracetamol-dose".to_string()),
                    title: "Paracetamol dose for children".to_string(),
                    ..Default::default()
                })
                .unwrap(),
                score: 0.9,
                named: false,
                popularity: 0.5,
                whole: false,
                learned: Some(LearnedPlace::Before("clevelandclinic.org".to_string())),
            },
            at: 0,
            under: None,
        };
        let mut hits = listed();
        let mut placed = vec![page.clone()];
        sources_lead(
            "NHS paracetamol for children dose age weight",
            &mut hits,
            &mut placed,
            &|_| true,
        );
        assert_eq!(
            domains(&hits),
            [
                "www.nhs.uk",
                "clevelandclinic.org",
                "medlineplus.gov",
                "mayoclinic.org",
                "pedi-stat.com"
            ]
        );
        let scores: Vec<f32> = hits.iter().map(|hit| hit.score).collect();
        assert_eq!(scores, [0.9, 0.8, 0.7, 0.6, 0.5]);
        // The page listed above the sites stays above them.
        assert_eq!(placed[0].at, 0);
        assert_eq!(
            placed[0].hit.learned,
            Some(LearnedPlace::Before("www.nhs.uk".to_string()))
        );

        // No source asked for, an operator, or not medical: as it was.
        for query in [
            "paracetamol for children dose age weight",
            "paracetamol dose site:nhs.uk",
            "nhs jobs",
        ] {
            let mut hits = listed();
            let mut placed = vec![page.clone()];
            sources_lead(query, &mut hits, &mut placed, &|_| true);
            assert_eq!(domains(&hits), domains(&listed()), "{query}");
            assert_eq!(placed, vec![page.clone()], "{query}");
        }
    }
}
