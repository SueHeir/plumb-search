//! Learned ranking: a small model, trained on the test searches in
//! `eval/`, that puts the first results of a search in a better order.
//!
//! The hand-made ranking ([`crate::Searcher`] and
//! [`crate::pages::place_pages`]) lists sites and pages. The model reads
//! the first [`LEARNED_ROWS`] of them, each with the signals that ranked it
//! (its place, score, text match, link score, whether it is named, the
//! pages under a site), and scores every row. The rows are then listed by
//! that score. It is gradient-boosted trees trained with LambdaMART
//! (`plumb-tools train-rank`), which learns from which row each test
//! search expects first; no search logs, only the labelled test searches.

use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::pages::{LearnedPlace, PageHit, PlacedPage};
use crate::Hit;

mod net;
pub use net::{squash, Layer, Net};

/// How many of the first listed rows the model puts in order.
pub const LEARNED_ROWS: usize = 10;

/// Page sets the model tells apart, by the start of their name
/// ([`crate::pages::Page::set`]).
const SETS: [&str; 8] = [
    "wikipedia",
    "stackoverflow",
    "stackexchange",
    "github",
    "books",
    "papers",
    "packages",
    "podcasts",
];

/// The signals of one page, as the model reads them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PageSignals {
    pub set: String,
    pub score: f32,
    pub named: bool,
    pub popularity: f32,
    pub whole: bool,
}

impl PageSignals {
    fn of(hit: &PageHit) -> Self {
        PageSignals {
            set: hit.page.set.clone(),
            score: hit.score,
            named: hit.named,
            popularity: hit.popularity,
            whole: hit.whole,
        }
    }
}

/// One listed row, as the model reads it: a site with the pages shown
/// under it, or a page listed on its own. `plumb-tools eval --features-out`
/// writes rows in this shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RowSignals {
    /// `site` or `page`.
    pub kind: String,
    /// A page's set; empty for a site.
    pub set: String,
    pub score: f32,
    /// A site's text match.
    pub text_score: f32,
    /// A site's text match without the search instruction, when the
    /// meaning of the query was found with one ([`Hit::placing_text_score`]).
    pub placing_text_score: Option<f32>,
    pub link_score: f32,
    pub named: bool,
    pub official: bool,
    pub demand: Option<f32>,
    /// A site's country, `None` for global sites.
    pub country: Option<String>,
    pub title: Option<String>,
    /// A page's popularity.
    pub popularity: f32,
    /// A page the query asks for in full.
    pub whole: bool,
    /// Pages shown under a site.
    pub under: Vec<PageSignals>,
    /// Diagnostic evidence only. This does not add model input features
    /// or change the compatible built-in model contract.
    pub query_evidence: Option<crate::CandidateEvidence>,
    /// In training data: 1 when the row holds an expected answer.
    pub label: u8,
}

impl RowSignals {
    /// The row of a site result and the pages under it.
    pub fn site(hit: &Hit, under: &[&PageHit]) -> Self {
        RowSignals {
            kind: "site".into(),
            score: hit.score,
            text_score: hit.text_score,
            placing_text_score: hit.placing_text_score,
            link_score: hit.link_score,
            named: hit.named,
            official: hit.official,
            demand: hit.demand,
            country: hit.country.clone(),
            title: hit.title.clone(),
            query_evidence: hit.query_evidence.clone(),
            under: under.iter().map(|p| PageSignals::of(p)).collect(),
            ..RowSignals::default()
        }
    }

    /// The row of a page listed on its own.
    pub fn page(hit: &PageHit) -> Self {
        RowSignals {
            kind: "page".into(),
            set: hit.page.set.clone(),
            score: hit.score,
            named: hit.named,
            popularity: hit.popularity,
            whole: hit.whole,
            ..RowSignals::default()
        }
    }

    fn is_page(&self) -> bool {
        self.kind == "page"
    }
}

/// The names of the values [`features`] gives each row, in order.
pub const FEATURES: [&str; 31] = [
    "pos_inv",
    "pos",
    "nwords",
    "question",
    "is_page",
    "set_wikipedia",
    "set_stackoverflow",
    "set_stackexchange",
    "set_github",
    "set_books",
    "set_papers",
    "set_packages",
    "set_podcasts",
    "p_score",
    "p_named",
    "p_pop",
    "p_whole",
    "s_score",
    "s_score_gap",
    "s_text",
    "s_link",
    "s_link_gap",
    "s_named",
    "s_official",
    "s_demand",
    "s_country",
    "s_under",
    "s_under_named",
    "s_under_pop",
    "s_has_title",
    "s_text_plain",
];

/// What the model reads of each of `rows`, listed in this order for
/// `query`: one value per name of [`FEATURES`]. Values a row does not
/// have (a page's text match) are 0.
pub fn features(query: &str, rows: &[RowSignals]) -> Vec<[f64; FEATURES.len()]> {
    let words = query.split_whitespace().count() as f64;
    let question = f64::from(u8::from(crate::pages::asked_as_question(query)));
    let sites = || rows.iter().filter(|r| !r.is_page());
    let best_score = sites().map(|r| f64::from(r.score)).fold(0.0, f64::max);
    let best_link = sites().map(|r| f64::from(r.link_score)).fold(0.0, f64::max);
    let flag = |b: bool| f64::from(u8::from(b));
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let mut f = [0.0; FEATURES.len()];
            let pos = (i + 1) as f64;
            f[0] = 1.0 / pos;
            f[1] = pos;
            f[2] = words;
            f[3] = question;
            f[4] = flag(row.is_page());
            if row.is_page() {
                if let Some(set) = SETS.iter().position(|s| row.set.starts_with(s)) {
                    f[5 + set] = 1.0;
                }
                f[13] = f64::from(row.score);
                f[14] = flag(row.named);
                f[15] = f64::from(row.popularity);
                f[16] = flag(row.whole);
            } else {
                let score = f64::from(row.score);
                let link = f64::from(row.link_score);
                f[17] = score;
                f[18] = best_score - score;
                f[19] = f64::from(row.text_score);
                f[20] = link;
                f[21] = best_link - link;
                f[22] = flag(row.named);
                f[23] = flag(row.official);
                f[24] = row.demand.map_or(-1.0, f64::from);
                f[25] = flag(row.country.is_some());
                // The site's own page a query names under it
                // ([`crate::pages::PageSearcher::add_named_site_page`]) is
                // not read: the model learned from sites with articles.
                let under = || {
                    row.under
                        .iter()
                        .filter(|p| !crate::pages::is_site_page_set(&p.set))
                };
                f[26] = under().count() as f64;
                f[27] = flag(under().any(|p| p.named));
                f[28] = under()
                    .map(|p| f64::from(p.popularity))
                    .fold(-1.0, f64::max);
                let titled = row.title.as_deref().is_some_and(|t| !t.trim().is_empty());
                f[29] = flag(titled);
                f[30] = f64::from(row.placing_text_score.unwrap_or(row.text_score));
            }
            f
        })
        .collect()
}

/// One regression tree: `x[feature] <= threshold` goes left.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Node {
    Leaf {
        value: f64,
    },
    Split {
        feature: usize,
        threshold: f64,
        left: Box<Node>,
        right: Box<Node>,
    },
}

impl Node {
    /// The value of the leaf that features `x` reach.
    pub fn value(&self, x: &[f64]) -> f64 {
        match self {
            Node::Leaf { value } => *value,
            Node::Split {
                feature,
                threshold,
                left,
                right,
            } => {
                if x[*feature] <= *threshold {
                    left.value(x)
                } else {
                    right.value(x)
                }
            }
        }
    }
}

/// A learned ranking: trees whose values add up to a row's score, or a
/// small neural network ([`Net`]) that gives it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    /// [`FEATURES`] when the model was trained, to refuse a model made
    /// for other features.
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    trees: Vec<Node>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    net: Option<Net>,
}

impl Model {
    /// A model of `trees` and `net` for today's [`FEATURES`].
    pub fn new(trees: Vec<Node>, net: Option<Net>) -> Self {
        Model {
            features: FEATURES.iter().map(|f| f.to_string()).collect(),
            trees,
            net,
        }
    }

    /// The model in `json` (as `plumb-tools train-rank` writes it).
    pub fn from_json(json: &str) -> Result<Self> {
        let model: Model = serde_json::from_str(json).context("reading a ranking model")?;
        if model.features != FEATURES {
            bail!("the ranking model was made for other features");
        }
        Ok(model)
    }

    /// The model every node ranks with, trained on `eval/`
    /// (`plumb-tools train-rank`).
    pub fn builtin() -> &'static Model {
        static MODEL: OnceLock<Model> = OnceLock::new();
        MODEL.get_or_init(|| {
            Model::from_json(include_str!("learned_model.json"))
                .expect("the built-in ranking model reads")
        })
    }

    /// The score of a row with features `x`; higher comes first.
    pub fn score(&self, x: &[f64]) -> f64 {
        let trees: f64 = self.trees.iter().map(|tree| tree.value(x)).sum();
        trees + self.net.as_ref().map_or(0.0, |net| net.score(x))
    }

    /// The order the model lists `rows` of `query` in: indexes into
    /// `rows`, best first. Only the first [`LEARNED_ROWS`] move; ties
    /// keep their order.
    pub fn order(&self, query: &str, rows: &[RowSignals]) -> Vec<usize> {
        let top = rows.len().min(LEARNED_ROWS);
        let scores: Vec<f64> = features(query, &rows[..top])
            .iter()
            .map(|x| self.score(x))
            .collect();
        let mut order: Vec<usize> = (0..top).collect();
        order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
        order.extend(top..rows.len());
        order
    }
}

/// One row as listed: a site (by index into the hits) or a page listed
/// on its own (by index into the placed pages).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Listed {
    Site(usize),
    Page(usize),
}

/// The rows `hits` and `placed` list, in order, as
/// [`crate::pages::place_pages`] places them: pages on their own before
/// the site at their `at`, after all sites when past the end.
fn listed(hits: &[Hit], placed: &[PlacedPage]) -> Vec<Listed> {
    let mut rows = Vec::with_capacity(hits.len() + placed.len());
    let alone = |at: usize| {
        placed
            .iter()
            .enumerate()
            .filter(move |(_, p)| p.under.is_none() && p.at == at)
            .map(|(i, _)| Listed::Page(i))
    };
    for i in 0..hits.len() {
        rows.extend(alone(i));
        rows.push(Listed::Site(i));
    }
    rows.extend(
        placed
            .iter()
            .enumerate()
            .filter(|(_, p)| p.under.is_none() && p.at >= hits.len())
            .map(|(i, _)| Listed::Page(i)),
    );
    rows
}

/// How close in meaning a site must be to speak for it when its words
/// do not, as [`crate::RankConfig::meaning_only_relevance`] is by default.
const NEAR_IN_MEANING: f32 = 0.35;

/// The text match under which the hand-made ranking gives a site the
/// query does not name only part of its popularity, as
/// [`crate::RankConfig::described_relevance`] does by default.
const WEAK_TEXT: f32 = 0.04;

/// Whether only popularity speaks for `hit`, the popularity the model
/// reads whole: the query names neither the site nor a page listed under
/// it, matches its text under [`WEAK_TEXT`] (often only in link text on
/// its homepage) and is not near it in meaning. A site with no evidence
/// noted, one the ranking did not find, is not such a site.
fn unsupported(hit: &Hit, placed: &[PlacedPage]) -> bool {
    !hit.named
        && hit.text_score < WEAK_TEXT
        && hit.query_evidence.as_ref().is_some_and(|e| {
            !e.full_name
                && !e.typed_domain
                && e.semantic_closeness.is_none_or(|s| s < NEAR_IN_MEANING)
        })
        && !placed
            .iter()
            .any(|p| p.hit.named && p.under.as_deref() == Some(hit.domain.as_str()))
}

/// The site rows or page rows `rows`, in the model's order, with each one
/// `bare` says only popularity speaks for moved down until every other row
/// above it in the hand-made order (a lower row) is listed. The others
/// keep their order.
fn after_supported(rows: Vec<usize>, bare: impl Fn(usize) -> bool) -> Vec<usize> {
    let supported: Vec<usize> = rows.iter().copied().filter(|&row| !bare(row)).collect();
    let mut listed = Vec::with_capacity(rows.len());
    let mut waiting = Vec::new();
    for row in rows {
        waiting.push(row);
        while let Some(at) = waiting
            .iter()
            .position(|&row| !bare(row) || supported.iter().all(|s| *s > row || listed.contains(s)))
        {
            listed.push(waiting.remove(at));
        }
    }
    listed
}

/// Puts the first rows of `query`'s results in the order `model` gives:
/// reorders `hits` and moves the pages listed on their own in `placed`.
/// Pages under a site stay under it.
///
/// The moved sites trade scores so that scores still go down the list:
/// what a node adds later (a browser's own likes, results from the
/// network) sorts by score and keeps this order. Each page listed on its
/// own notes where it went ([`PageHit::learned`]), which
/// [`crate::pages::place_pages`] keeps when the results are placed again.
pub fn reorder(model: &Model, query: &str, hits: &mut Vec<Hit>, placed: &mut Vec<PlacedPage>) {
    let rows = listed(hits, placed);
    if rows.len() < 2 {
        return;
    }
    let signals: Vec<RowSignals> = rows
        .iter()
        .map(|row| match *row {
            Listed::Site(i) => {
                let under: Vec<&PageHit> = placed
                    .iter()
                    .filter(|p| p.under.as_deref() == Some(hits[i].domain.as_str()))
                    .map(|p| &p.hit)
                    .collect();
                RowSignals::site(&hits[i], &under)
            }
            Listed::Page(i) => RowSignals::page(&placed[i].hit),
        })
        .collect();
    let mut order = model.order(query, &signals);
    // The model may reorder within a relevance tier, but an unsupported
    // domain word cannot regain top placement because it is popular.
    // A named page's corroborated official-site identity is also query
    // evidence. Account for it without rewriting the original site-query
    // diagnostics or adding learned input features.
    // Sites and pages keep their model slots; only who fills them changes.
    let page_site = crate::pages::named_page_site(hits, placed.iter().map(|page| &page.hit));
    let mut sites: Vec<_> = order
        .iter()
        .copied()
        .filter(|&row| matches!(rows[row], Listed::Site(_)))
        .collect();
    sites.sort_by_key(|&row| {
        std::cmp::Reverse(match rows[row] {
            Listed::Site(i) => {
                let tier = hits[i]
                    .query_evidence
                    .as_ref()
                    .map_or(1, |evidence| evidence.relevance_tier);
                tier.max(u8::from(page_site == Some(i)))
            }
            Listed::Page(_) => unreachable!(),
        })
    });
    // Nor can popularity alone lift a site past one the hand-made ranking
    // put above it, which with tiers off (the default) is all that stops
    // it: "tom holland" put thenation.com, which has the name only in its
    // homepage's link text, before tom-holland.org.
    let bare = |row: usize| match rows[row] {
        Listed::Site(i) => page_site != Some(i) && unsupported(&hits[i], placed),
        Listed::Page(_) => false,
    };
    let mut sites = after_supported(sites, bare).into_iter();
    // Nor can popularity alone lift a site's inner page the query does not
    // name past a page listed above it: such a page's popularity is its
    // site's weight, which the model reads as the page being much read. "wolfram code" put MathWorld's
    // "Barker Code" before the article Wolfram code, "cellular automaton"
    // its "Additive Cellular Automaton" before Cellular automaton.
    let inner = |row: usize| match rows[row] {
        Listed::Page(i) => {
            let hit = &placed[i].hit;
            hit.page.is_site_page() && !hit.named && !hit.whole
        }
        Listed::Site(_) => false,
    };
    let pages = order
        .iter()
        .copied()
        .filter(|&row| matches!(rows[row], Listed::Page(_)))
        .collect();
    let mut pages = after_supported(pages, inner).into_iter();
    for row in &mut order {
        *row = match rows[*row] {
            Listed::Site(_) => sites.next().expect("site slot"),
            Listed::Page(_) => pages.next().expect("page slot"),
        };
    }
    let mut new_hits: Vec<Hit> = Vec::with_capacity(hits.len());
    let mut alone: Vec<PlacedPage> = Vec::new();
    for &row in &order {
        match rows[row] {
            Listed::Site(i) => new_hits.push(hits[i].clone()),
            Listed::Page(i) => {
                let mut page = placed[i].clone();
                page.at = new_hits.len();
                alone.push(page);
            }
        }
    }
    // A docs page found by its words, or another site's page while the
    // query names the best site's owner, or the best site with one of its
    // pages, never comes before the best site but its own, whatever the
    // model says: "python package index" wants pypi.org first, "minecraft"
    // minecraft.net.
    let names_best = crate::pages::named_page_site(&new_hits, placed.iter().map(|p| &p.hit))
        == Some(0)
        || crate::pages::names_best_site_page(&new_hits, placed);
    for page in alone
        .iter_mut()
        .filter(|p| p.at == 0 && crate::pages::kept_below(&p.hit, &new_hits, names_best))
    {
        page.at = 1;
    }
    // The moved sites' scores, highest first, in their new order.
    let moved = new_hits
        .iter()
        .zip(hits.iter())
        .rposition(|(new, old)| new.domain != old.domain)
        .map_or(0, |last| last + 1);
    let mut scores: Vec<f32> = new_hits[..moved].iter().map(|h| h.score).collect();
    scores.sort_by(|a, b| b.total_cmp(a));
    for (hit, score) in new_hits.iter_mut().zip(scores) {
        hit.score = score;
    }
    for page in &mut alone {
        page.hit.learned = Some(match new_hits.get(page.at) {
            Some(site) => LearnedPlace::Before(site.domain.clone()),
            None => LearnedPlace::Last,
        });
    }
    let under = placed.iter().filter(|p| p.under.is_some()).cloned();
    *placed = alone.into_iter().chain(under).collect();
    *hits = new_hits;
    crate::pages::keep_page_rules(query, hits, placed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::{place_pages, Page, PlacedPage};
    use plumb_core::article::Article;

    fn site(domain: &str, score: f32, named: bool) -> Hit {
        Hit {
            domain: domain.into(),
            url: format!("https://{domain}/"),
            title: Some(domain.into()),
            description: None,
            score,
            text_score: 1.0,
            link_score: 0.5,
            country: None,
            named,
            official: false,
            key_pages: Vec::new(),
            demand: None,
            missing_words: false,
            query_evidence: None,
            placing_text_score: None,
        }
    }

    fn article(title: &str, named: bool) -> PageHit {
        PageHit {
            page: Page::from_article(
                "en",
                Article {
                    title: title.into(),
                    views: 1_000,
                    ..Article::default()
                },
            ),
            score: 0.5,
            named,
            popularity: 0.9,
            whole: false,
            learned: None,
        }
    }

    fn evidence(domain: &str, relevance_tier: u8) -> crate::CandidateEvidence {
        crate::CandidateEvidence {
            domain: domain.into(),
            lexical_score: 0.3,
            semantic_closeness: None,
            query_coverage: 0.3,
            substantive_coverage: 0.3,
            remaining_coverage: 0.3,
            named_subject_words: 0,
            named_subject_domain: None,
            named_subject_reason: None,
            subject_coverage: None,
            subject_remaining_coverage: None,
            full_name: false,
            typed_domain: false,
            partial_name_words: 1,
            relevance_tier,
            source_quality: None,
        }
    }

    // A constant model preserves the input order, isolating the guard's
    // effective tier from whatever the learned model happens to favor.
    fn tied_model() -> Model {
        Model::new(Vec::new(), None)
    }

    /// Puts a named page first, then a named site, as a model trained on
    /// searches that expect them does.
    fn named_first_model() -> Model {
        let feature = |name| FEATURES.iter().position(|f| *f == name).unwrap();
        let leaf = |value| Box::new(Node::Leaf { value });
        let named_site = Node::Split {
            feature: feature("s_named"),
            threshold: 0.5,
            left: leaf(0.0),
            right: leaf(1.0),
        };
        let tree = Node::Split {
            feature: feature("p_named"),
            threshold: 0.5,
            left: Box::new(named_site),
            right: leaf(2.0),
        };
        Model::new(vec![tree], None)
    }

    #[test]
    fn named_page_identity_contributes_to_the_effective_relevance_tier() {
        for repository in [false, true] {
            let mut rival = site("music.example", 1.0, false);
            rival.query_evidence = Some(evidence(&rival.domain, 1));
            let mut official = site("youtube.com", 0.9, repository);
            official.official = !repository;
            official.query_evidence = Some(evidence(&official.domain, 0));
            let original = official.query_evidence.clone();
            let mut named = article("YouTube Music", true);
            named.page.site = Some(official.domain.clone());
            if repository {
                named.page.set = crate::pages::GITHUB_SET.into();
            } else {
                named.page.item = Some("Q28404534".into());
            }
            let mut hits = vec![rival, official];
            crate::pages::lift_named_sites(&mut hits, &[named.clone()]);
            assert_eq!(hits[0].domain, "youtube.com");
            let mut placed = place_pages("youtube music", &hits, vec![named]);
            reorder(&tied_model(), "youtube music", &mut hits, &mut placed);
            assert_eq!(hits[0].domain, "youtube.com", "repository={repository}");
            // Page identity changes guard eligibility, not the original
            // site-query diagnostics or the learned feature contract.
            assert_eq!(hits[0].query_evidence, original);
            assert_eq!(placed[0].under.as_deref(), Some("youtube.com"));
        }
    }

    #[test]
    fn unconfirmed_page_identity_does_not_raise_the_relevance_tier() {
        for control in [
            "partial",
            "unofficial",
            "namesake",
            "film",
            "borrowed",
            "repo",
        ] {
            let mut weak = site("youtube.com", 1.0, false);
            weak.official = true;
            weak.query_evidence = Some(evidence(&weak.domain, 0));
            let original = weak.query_evidence.clone();
            let mut supported = site("music.example", 0.9, false);
            supported.query_evidence = Some(evidence(&supported.domain, 1));
            let mut named = article("YouTube Music", true);
            named.page.item = Some("Q28404534".into());
            named.page.site = Some(weak.domain.clone());
            match control {
                "partial" => named.named = false,
                "unofficial" => weak.official = false,
                "namesake" => named.page.site = Some("youtube-music.example".into()),
                "film" => named.page.set = crate::pages::FILMS_SET.into(),
                "borrowed" => {
                    weak.title = Some(named.page.title.clone());
                    named.page.site = None;
                }
                "repo" => {
                    named.page.set = crate::pages::GITHUB_SET.into();
                    named.page.item = None;
                }
                _ => unreachable!(),
            }
            let mut hits = vec![weak, supported];
            let mut placed = place_pages("youtube music", &hits, vec![named]);
            reorder(&tied_model(), "youtube music", &mut hits, &mut placed);
            assert_eq!(hits[0].domain, "music.example", "{control}");
            assert_eq!(hits[1].query_evidence, original, "{control}");
        }
    }

    /// Puts the most linked-to site first, whatever its text match, as
    /// the built-in model did with thenation.com for "tom holland".
    fn popular_first_model() -> Model {
        let tree = Node::Split {
            feature: FEATURES.iter().position(|f| *f == "s_link").unwrap(),
            threshold: 0.55,
            left: Box::new(Node::Leaf { value: 0.0 }),
            right: Box::new(Node::Leaf { value: 1.0 }),
        };
        Model::new(vec![tree], None)
    }

    #[test]
    fn popularity_alone_lifts_no_site_past_one_ranked_above_it() {
        let query = "tom holland";
        let popular = |text_score: f32, closeness: Option<f32>| {
            let mut hit = site("thenation.com", 0.07, false);
            hit.text_score = text_score;
            hit.link_score = 0.6;
            hit.query_evidence = Some(crate::CandidateEvidence {
                semantic_closeness: closeness,
                ..evidence(&hit.domain, 0)
            });
            hit
        };
        let order_of = |first: Option<Hit>, popular: Hit, pages: Vec<PageHit>| {
            let mut namesake = site("tom-holland.org", 0.96, true);
            namesake.link_score = 0.2;
            let mut words = site("tomhollandbooks.com", 0.3, false);
            words.text_score = 0.5;
            let mut hits: Vec<Hit> = first.into_iter().collect();
            hits.extend([namesake, words, popular, site("other.com", 0.05, false)]);
            for hit in hits.iter_mut().filter(|hit| hit.query_evidence.is_none()) {
                hit.query_evidence = Some(evidence(&hit.domain, 0));
            }
            let mut placed = place_pages(query, &hits, pages);
            reorder(&popular_first_model(), query, &mut hits, &mut placed);
            hits.into_iter().map(|hit| hit.domain).collect::<Vec<_>>()
        };
        // Found by a link with the name on its homepage, or by too little
        // of its text ("gauss" in stonybrook.edu's), with or without a
        // vector, it stays below the sites ranked above it.
        for (text, closeness) in [(0.014, Some(0.0)), (0.03, Some(0.2)), (0.022, None)] {
            let order = order_of(None, popular(text, closeness), Vec::new());
            assert_eq!(
                order[..3],
                ["tom-holland.org", "tomhollandbooks.com", "thenation.com"],
                "{text} {closeness:?}"
            );
        }
        // Its text matching, being near in meaning or holding the page
        // the query names lifts it.
        let mut article = article("Tom Holland", true);
        article.page.site = Some("thenation.com".into());
        for (hit, pages) in [
            (popular(0.3, Some(0.0)), Vec::new()),
            (popular(0.014, Some(0.8)), Vec::new()),
            (popular(0.014, Some(0.0)), vec![article]),
        ] {
            assert_eq!(order_of(None, hit, pages)[0], "thenation.com");
        }
        // Already above the others, it keeps its place.
        let first = Some(popular(0.014, Some(0.0)));
        let order = order_of(first, site("x.com", 0.0, false), Vec::new());
        assert_eq!(order[0], "thenation.com");
    }

    #[test]
    fn popularity_alone_lifts_no_inner_page_past_a_page_above_it() {
        // Lists much-read pages first, then much-linked sites, as the
        // built-in model did with MathWorld's "Barker Code" for "wolfram
        // code", whose popularity is MathWorld's weight.
        let feature = |name| FEATURES.iter().position(|f| *f == name).unwrap();
        let leaf = |value| Box::new(Node::Leaf { value });
        let linked = Node::Split {
            feature: feature("s_link"),
            threshold: 0.4,
            left: leaf(0.0),
            right: leaf(1.0),
        };
        let tree = Node::Split {
            feature: feature("p_pop"),
            threshold: 0.5,
            left: Box::new(linked),
            right: leaf(2.0),
        };
        let model = Model::new(vec![tree], None);
        let mut code = article("Wolfram code", true);
        (code.score, code.popularity) = (0.65, 0.3);
        code.page.item = Some("Q8030198".into());
        let inner = |title: &str, named: bool, whole: bool| PageHit {
            page: Page::from_set(
                crate::pages::SUBPAGES_SET,
                Article {
                    title: title.into(),
                    item: Some("https://mathworld.wolfram.com/BarkerCode.html".into()),
                    views: 4_000,
                    ..Article::default()
                },
            )
            .unwrap(),
            score: 0.59,
            named,
            popularity: 0.968,
            whole,
            learned: None,
        };
        let order_of = |pages: Vec<PageHit>| {
            let mut hits = vec![
                site("wolfram.com", 0.76, false),
                site("wolframalpha.com", 0.67, false),
            ];
            let mut placed: Vec<PlacedPage> = pages
                .into_iter()
                .map(|hit| PlacedPage {
                    at: 0,
                    under: None,
                    hit,
                })
                .collect();
            reorder(&model, "wolfram code", &mut hits, &mut placed);
            listed(&hits, &placed)
                .into_iter()
                .map(|row| match row {
                    Listed::Site(i) => hits[i].domain.clone(),
                    Listed::Page(i) => placed[i].hit.page.title.clone(),
                })
                .collect::<Vec<_>>()
        };
        let barker = "Barker Code -- from Wolfram MathWorld";
        // The article listed above it takes its place.
        assert_eq!(
            order_of(vec![code.clone(), inner(barker, false, false)]),
            ["Wolfram code", "wolfram.com", "wolframalpha.com", barker]
        );
        // Already above the article, named by the query or asked for
        // whole, it is left to the model.
        for (pages, first) in [
            (vec![inner(barker, false, false), code.clone()], barker),
            (
                vec![code.clone(), inner("Wolfram Code", true, false)],
                "Wolfram Code",
            ),
            (vec![code.clone(), inner(barker, false, true)], barker),
        ] {
            assert_eq!(order_of(pages)[0], first);
        }
    }

    #[test]
    fn a_model_for_other_features_is_refused() {
        assert!(Model::from_json(r#"{"features":["pos"],"trees":[]}"#).is_err());
    }

    #[test]
    fn the_builtin_model_reads() {
        let model = Model::builtin();
        assert!(!model.trees.is_empty());
    }

    #[test]
    fn reordered_results_stay_put_when_placed_again() {
        let model = named_first_model();
        let query = "query 0";
        let mut hits = vec![
            site("first.com", 1.0, false),
            site("named.com", 0.9, true),
            site("third.com", 0.8, false),
        ];
        let found = vec![article("Thing", true)];
        let mut placed = place_pages(query, &hits, found.clone());
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].under, None);
        reorder(&model, query, &mut hits, &mut placed);
        // The named article leads, before the named site.
        assert_eq!(placed[0].at, 0);
        assert_eq!(
            placed[0].hit.learned,
            Some(LearnedPlace::Before(hits[0].domain.clone()))
        );
        assert_eq!(hits[0].domain, "named.com");
        // Scores still go down the list.
        assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
        // Placing the pages again (as the results page does) keeps it first.
        let again = place_pages(query, &hits, placed.iter().map(|p| p.hit.clone()).collect());
        assert_eq!(again[0].at, 0);
    }

    #[test]
    fn a_docs_page_found_by_its_words_never_leads() {
        let model = named_first_model();
        let mut hits = vec![site("pypi.org", 1.0, false), site("python.org", 0.9, false)];
        let docs = PageHit {
            page: Page::from_docs(Article {
                title: "Software Packaging and Distribution".into(),
                item: Some("https://docs.python.org/3/library/distribution.html".into()),
                views: 1_000,
                ..Article::default()
            })
            .unwrap(),
            score: 0.9,
            named: false,
            popularity: 1.0,
            whole: true,
            learned: None,
        };
        let mut placed = vec![PlacedPage {
            at: 0,
            under: None,
            hit: docs,
        }];
        reorder(&model, "python package index", &mut hits, &mut placed);
        assert!(placed[0].at >= 1, "{:?}", placed[0].at);
        let expected = match hits.get(placed[0].at) {
            Some(site) => LearnedPlace::Before(site.domain.clone()),
            None => LearnedPlace::Last,
        };
        assert_eq!(placed[0].hit.learned, Some(expected));
    }

    #[test]
    fn another_sites_page_named_like_the_official_site_never_leads() {
        let model = named_first_model();
        let named = |set: &str, url: &str| PageHit {
            page: Page::from_set(
                set,
                Article {
                    title: "Minecraft".into(),
                    item: Some(url.into()),
                    views: 4_000,
                    ..Article::default()
                },
            )
            .unwrap(),
            score: 0.96,
            named: true,
            popularity: 0.92,
            whole: false,
            learned: None,
        };
        let led = |page: PageHit, official: bool| {
            let mut best = site("minecraft.net", 1.3, true);
            best.official = official;
            let mut hits = vec![best, site("minecraft.wiki", 1.0, true)];
            let mut placed = place_pages("minecraft", &hits, vec![page]);
            reorder(&model, "minecraft", &mut hits, &mut placed);
            assert_eq!(hits[0].domain, "minecraft.net");
            let expected = match hits.get(placed[0].at) {
                Some(site) => LearnedPlace::Before(site.domain.clone()),
                None => LearnedPlace::Last,
            };
            assert_eq!(placed[0].hit.learned, Some(expected));
            placed[0].at == 0
        };
        let museum = "https://computerhistory.org/minecraft/";
        // A museum's page on Minecraft, or a news site's, comes after
        // minecraft.net.
        assert!(!led(named(crate::pages::SUBPAGES_SET, museum), true));
        let news = "https://www.cnet.com/tech/gaming/minecraft/";
        assert!(!led(named(crate::pages::REFERENCE_SET, news), true));
        // The official site's own page, one named while the best site is
        // not an official one, and the article are left to the model.
        let own = "https://www.minecraft.net/en-us/about-minecraft";
        assert!(led(named(crate::pages::SUBPAGES_SET, own), true));
        assert!(led(named(crate::pages::SUBPAGES_SET, museum), false));
        let mut article = named(crate::pages::SUBPAGES_SET, museum);
        article.page = Page::from_article(
            "en",
            Article {
                title: "Minecraft".into(),
                views: 4_000,
                ..Article::default()
            },
        );
        assert!(led(article, true));
    }

    #[test]
    fn another_sites_page_found_for_what_the_best_site_is_of_never_leads() {
        // Lists whole pages first, as the model has for "red dead
        // redemption 2", whose IGN guide has all its words.
        let whole = Node::Split {
            feature: FEATURES.iter().position(|f| *f == "p_whole").unwrap(),
            threshold: 0.5,
            left: Box::new(Node::Leaf { value: 0.0 }),
            right: Box::new(Node::Leaf { value: 1.0 }),
        };
        let model = Model::new(vec![whole], None);
        let guide = PageHit {
            page: Page::from_set(
                crate::pages::SUBPAGES_SET,
                Article {
                    title: "Red Dead Redemption 2 Guide".into(),
                    item: Some("https://www.ign.com/wikis/red-dead-redemption-2".into()),
                    views: 4_000,
                    ..Article::default()
                },
            )
            .unwrap(),
            score: 0.56,
            named: false,
            popularity: 0.87,
            whole: true,
            learned: None,
        };
        let mut game = article("Red Dead Redemption 2", true);
        game.page.item = Some("Q20981587".into());
        game.page.site = Some("rockstargames.com".into());
        // The query names the game, so its official site, with the
        // article under it, comes first; naming nothing whose site it is,
        // the guide may lead.
        for names_game in [true, false] {
            let mut best = site("rockstargames.com", 1.3, false);
            best.official = true;
            let mut hits = vec![best, site("rdr2map.com", 1.0, false)];
            let mut pages = vec![guide.clone()];
            if names_game {
                pages.push(game.clone());
            }
            let query = "red dead redemption 2";
            let mut placed = place_pages(query, &hits, pages);
            reorder(&model, query, &mut hits, &mut placed);
            let at = |title: &str| placed.iter().find(|p| p.hit.page.title == title);
            assert_eq!(at(&guide.page.title).unwrap().at == 0, !names_game);
            if names_game {
                let game = at(&game.page.title).unwrap();
                assert_eq!(game.under.as_deref(), Some("rockstargames.com"));
            }
        }
    }

    #[test]
    fn sites_trade_scores_when_they_move() {
        let model = named_first_model();
        let query = "query 1";
        let mut hits = vec![
            site("first.com", 1.0, false),
            site("named.com", 0.9, true),
            site("third.com", 0.8, false),
        ];
        let mut placed = Vec::new();
        reorder(&model, query, &mut hits, &mut placed);
        let domains: Vec<&str> = hits.iter().map(|h| h.domain.as_str()).collect();
        assert_eq!(domains[0], "named.com");
        assert_eq!(hits[0].score, 1.0);
        assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
    }
}
