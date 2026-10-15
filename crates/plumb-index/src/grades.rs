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
/// and sorts again, keeping the order of equal scores. A `low_value` site
/// the query names keeps its score: someone searching for a little-known
/// service by name wants it. A `spam` site loses it even then, since spam
/// is often named by the words of links pointing to it ("ebay").
pub fn demote(grades: &SiteGrades, percent: u8, hits: &mut [Hit]) {
    let weight = f32::from(percent.min(100)) / 100.0;
    if weight == 0.0 || grades.is_empty() {
        return;
    }
    let mut changed = false;
    for hit in hits.iter_mut() {
        let Some(grade) = grades.get(&hit.domain) else {
            continue;
        };
        if grade == Grade::LowValue && hit.named {
            continue;
        }
        hit.score -= weight * grade.penalty() * hit.score.abs();
        changed = true;
    }
    if changed {
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
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
        let order = |hits: &[Hit]| hits.iter().map(|h| h.domain.clone()).collect::<Vec<_>>();

        let mut hits = ranked();
        demote(&grades, 0, &mut hits);
        assert_eq!(hits, ranked());

        let mut hits = ranked();
        demote(&grades, 100, &mut hits);
        assert_eq!(
            order(&hits),
            ["nfl.com", "signin.example", "espn.com", "nflbox.sx"]
        );
        assert!((hits[1].score - 0.63).abs() < 1e-6);

        let mut hits = ranked();
        demote(&grades, 50, &mut hits);
        assert_eq!(
            order(&hits),
            ["nfl.com", "signin.example", "nflbox.sx", "espn.com"]
        );
    }

    #[test]
    fn a_named_low_value_site_keeps_its_place_but_named_spam_does_not() {
        let grades = SiteGrades::new([
            ("datahubnet.online".to_string(), Grade::LowValue),
            ("ebay-deals.example".to_string(), Grade::Spam),
        ]);
        let mut hits = vec![
            hit("datahubnet.online", 1.0, true),
            hit("ebay-deals.example", 0.9, true),
            hit("ebay.com", 0.8, true),
        ];
        demote(&grades, 100, &mut hits);
        let order: Vec<_> = hits.iter().map(|h| h.domain.as_str()).collect();
        assert_eq!(
            order,
            ["datahubnet.online", "ebay.com", "ebay-deals.example"]
        );
    }
}
