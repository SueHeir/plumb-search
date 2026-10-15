//! Grades a language model gave sites, which push the sites down in the
//! results: `spam` (made to game search or mislead) and `low_value` (real,
//! but no use to anyone who doesn't already use it). The project publishes
//! them as the signed set `plumb/site-grades` (plumb-node's
//! `node::grades`); a site with no grade is fine.
//!
//! The grades are a demotion, not a filter: a graded site loses part of
//! its score, so it falls behind the sites that match as well, and out of
//! the results when enough others match. How much it loses is the node's
//! own choice ([`demote`]'s `percent`), so nodes can trust the grades as
//! much as they like.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::Hit;

/// How many more sites a search ranks when grades may push some out.
pub const MARGIN: usize = 10;

/// A site's grade. `ok` sites have none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Grade {
    LowValue,
    Spam,
}

impl Grade {
    /// The grade a set writes as `name` (`spam`, `low_value`); `None` for
    /// `ok` or a grade this version does not know.
    pub fn parse(name: &str) -> Option<Grade> {
        match name {
            "spam" => Some(Grade::Spam),
            "low_value" => Some(Grade::LowValue),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Grade::Spam => "spam",
            Grade::LowValue => "low_value",
        }
    }

    /// The share of its score a site with this grade loses at 100%.
    fn penalty(self) -> f32 {
        match self {
            Grade::Spam => 0.9,
            Grade::LowValue => 0.3,
        }
    }
}

/// Graded sites, as sorted hashes of their domains with their grades: 16
/// bytes a site. A site is its exact domain, as records and hits name it:
/// a grade of status.example.com says nothing of example.com.
#[derive(Debug, Default)]
pub struct SiteGrades(Vec<(u64, Grade)>);

impl SiteGrades {
    /// The grades of `sites`. A site graded twice keeps its worse grade.
    pub fn new(sites: impl IntoIterator<Item = (String, Grade)>) -> Self {
        let mut grades: Vec<(u64, Grade)> = sites
            .into_iter()
            .map(|(domain, grade)| (hash(&domain.to_ascii_lowercase()), grade))
            .collect();
        grades.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        grades.dedup_by_key(|(hash, _)| *hash);
        SiteGrades(grades)
    }

    /// The grade of the site `domain`.
    pub fn get(&self, domain: &str) -> Option<Grade> {
        let hash = hash(&domain.to_ascii_lowercase());
        self.0
            .binary_search_by(|(h, _)| h.cmp(&hash))
            .ok()
            .map(|at| self.0[at].1)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Takes `percent` (0 to 100) of each graded hit's penalty off its score
/// and moves the hit down to where its new score breaks the order least:
/// past the later hits that score above it, but not past more that score
/// below. Nothing else moves, and the hits placed by rule rather than
/// score (health authorities first, untitled sites last) neither hold a
/// graded hit up nor pull it down past the rest.
///
/// A site whose address is the query keeps its score, as does a
/// `low_value` site the query names (someone searching for a little-known
/// service by name wants it) or that Wikidata lists as an organization's
/// official site: the model judges the record, and an official site's
/// record can be a stand-in page a crawl got. A named or official `spam`
/// site loses it, since spam is often named by the words of links
/// pointing to it ("ebay") and a taken-over domain stays listed.
pub fn demote(grades: &SiteGrades, percent: u8, query: &str, hits: &mut [Hit]) {
    demote_by(grades, percent, query, hits, |hit| hit);
}

/// [`demote`] for results that each hold a hit, such as other nodes'.
pub fn demote_by<T>(
    grades: &SiteGrades,
    percent: u8,
    query: &str,
    results: &mut [T],
    hit: impl Fn(&mut T) -> &mut Hit,
) {
    let weight = f32::from(percent.min(100)) / 100.0;
    if weight == 0.0 || grades.is_empty() {
        return;
    }
    let typed = crate::names::typed_domain(query);
    let mut demoted = Vec::new();
    for (at, result) in results.iter_mut().enumerate() {
        let hit = hit(result);
        let Some(grade) = grades.get(&hit.domain) else {
            continue;
        };
        let addressed = typed.is_some() && typed == plumb_core::registrable_domain(&hit.domain);
        if addressed || grade == Grade::LowValue && (hit.named || hit.official) {
            continue;
        }
        hit.score -= weight * grade.penalty() * hit.score.abs();
        demoted.push(at);
    }
    // The last first, so each moves past hits already in their places.
    for at in demoted.into_iter().rev() {
        let score = hit(&mut results[at]).score;
        let later: Vec<f32> = results[at + 1..]
            .iter_mut()
            .map(|result| hit(result).score)
            .collect();
        // How many later hits are out of order with it, where it is and
        // after each it could move past.
        let mut wrong = later.iter().filter(|&&other| other > score).count();
        let (mut past, mut least) = (0, wrong);
        for (i, &other) in later.iter().enumerate() {
            if other > score {
                wrong -= 1;
            } else if other < score {
                wrong += 1;
            }
            if wrong < least {
                (past, least) = (i + 1, wrong);
            }
        }
        results[at..=at + past].rotate_left(1);
    }
}

fn hash(domain: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    domain.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(domain: &str, score: f32, named: bool) -> Hit {
        Hit {
            domain: domain.into(),
            url: format!("https://{domain}/"),
            title: None,
            description: None,
            score,
            text_score: 0.0,
            link_score: 0.0,
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

    fn order(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.domain.as_str()).collect()
    }

    fn grades() -> SiteGrades {
        SiteGrades::new([
            ("nflbox.sx".to_string(), Grade::Spam),
            ("thin.example".to_string(), Grade::LowValue),
            ("THIN.example".to_string(), Grade::Spam),
        ])
    }

    #[test]
    fn a_site_is_found_by_its_exact_domain_and_keeps_its_worse_grade() {
        let grades = grades();
        assert_eq!(grades.len(), 2);
        assert_eq!(grades.get("NFLBOX.sx"), Some(Grade::Spam));
        assert_eq!(grades.get("live.nflbox.sx"), None);
        assert_eq!(grades.get("thin.example"), Some(Grade::Spam));
        assert_eq!(grades.get("nba.com"), None);
        assert_eq!(Grade::parse("ok"), None);
        assert_eq!(Grade::parse(Grade::LowValue.name()), Some(Grade::LowValue));
    }

    #[test]
    fn graded_sites_fall_behind_by_the_share_the_node_chose() {
        let grades = SiteGrades::new([
            ("nflbox.sx".to_string(), Grade::Spam),
            ("signin.example".to_string(), Grade::LowValue),
        ]);
        let ranked = || {
            vec![
                hit("nflbox.sx", 1.0, false),
                hit("signin.example", 0.9, false),
                hit("nfl.com", 0.8, false),
                hit("espn.com", 0.5, false),
            ]
        };
        let mut hits = ranked();
        demote(&grades, 0, "nfl", &mut hits);
        assert_eq!(hits, ranked());

        let mut hits = ranked();
        demote(&grades, 100, "nfl", &mut hits);
        assert_eq!(
            order(&hits),
            ["nfl.com", "signin.example", "espn.com", "nflbox.sx"]
        );
        assert!((hits[1].score - 0.63).abs() < 1e-6);

        let mut hits = ranked();
        demote(&grades, 50, "nfl", &mut hits);
        assert_eq!(
            order(&hits),
            ["nfl.com", "signin.example", "nflbox.sx", "espn.com"]
        );
    }

    #[test]
    fn a_named_or_official_low_value_site_keeps_its_place_but_spam_does_not() {
        let grades = SiteGrades::new([
            ("datahubnet.online".to_string(), Grade::LowValue),
            ("manhattanda.org".to_string(), Grade::LowValue),
            ("ebay-deals.example".to_string(), Grade::Spam),
        ]);
        let official = |domain: &str, score: f32| Hit {
            official: true,
            ..hit(domain, score, false)
        };
        let mut hits = vec![
            hit("datahubnet.online", 1.0, true),
            official("manhattanda.org", 0.95),
            hit("ebay-deals.example", 0.9, true),
            hit("ebay.com", 0.8, true),
        ];
        demote(&grades, 100, "ebay", &mut hits);
        assert_eq!(
            order(&hits),
            [
                "datahubnet.online",
                "manhattanda.org",
                "ebay.com",
                "ebay-deals.example"
            ]
        );
    }

    #[test]
    fn only_graded_sites_move_rule_placed_ones_hold_no_place_and_addresses_none() {
        let grades = SiteGrades::new([("rxpills.top".to_string(), Grade::Spam)]);
        // An authority the ranking put first by rule, with a lower score.
        let ranked = || {
            vec![
                hit("nhs.uk", 0.0, false),
                hit("rxpills.top", 0.8, false),
                hit("webmd.com", 0.5, false),
                hit("mayoclinic.org", 0.05, false),
            ]
        };
        let mut hits = ranked();
        demote(&grades, 100, "ibuprofen", &mut hits);
        assert_eq!(
            order(&hits),
            ["nhs.uk", "webmd.com", "rxpills.top", "mayoclinic.org"]
        );

        let mut hits = ranked();
        demote(&grades, 100, "www.rxpills.top", &mut hits);
        assert_eq!(hits, ranked());

        // Nor is a graded hit pulled down past lower ones by a site put
        // last by rule, here one with no title.
        let grades = SiteGrades::new([("thin.example".to_string(), Grade::LowValue)]);
        let mut hits = vec![
            hit("first.example", 1.0, false),
            hit("thin.example", 0.95, false),
            hit("a.example", 0.6, false),
            hit("b.example", 0.1, false),
            hit("untitled.example", 0.8, false),
        ];
        demote(&grades, 100, "thin", &mut hits);
        assert_eq!(order(&hits)[1], "thin.example");

        // Nor held up by a hit put second with no score, as a spelling's.
        let grades = SiteGrades::new([("rxpills.top".to_string(), Grade::Spam)]);
        let mut hits = vec![
            hit("rxpills.top", 0.9, false),
            hit("spelled.example", 0.0, false),
            hit("a.example", 0.8, false),
            hit("b.example", 0.6, false),
        ];
        demote(&grades, 100, "rx pills", &mut hits);
        assert_eq!(
            order(&hits),
            ["spelled.example", "a.example", "b.example", "rxpills.top"]
        );
    }
}
