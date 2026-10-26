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
//! search expects first and from an LLM's grades of every row of other
//! searches; no search logs.

use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::pages::{LearnedPlace, PageHit, PlacedPage};
use crate::Hit;

mod net;
pub use net::{squash, Layer, Net};

/// How many of the first listed rows the model puts in order.
pub const LEARNED_ROWS: usize = 10;

/// How many of a search's first sites the site model puts in order
/// ([`reorder_sites`]), before pages are placed among them.
pub const LEARNED_SITES: usize = 100;

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

/// Page sets told apart since, whose values follow the others.
const LATER_SETS: [&str; 3] = ["reference", "subpages", "docs"];

/// The signals of one page, as the model reads them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PageSignals {
    pub set: String,
    pub title: String,
    pub score: f32,
    pub named: bool,
    pub popularity: f32,
    pub whole: bool,
}

impl PageSignals {
    fn of(hit: &PageHit) -> Self {
        PageSignals {
            set: hit.page.set.clone(),
            title: hit.page.title.clone(),
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
    /// A site's domain or a page's address.
    pub key: String,
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
    pub description: Option<String>,
    /// A page's popularity.
    pub popularity: f32,
    /// A page the query asks for in full.
    pub whole: bool,
    /// Pages shown under a site.
    pub under: Vec<PageSignals>,
    /// How a site matches the query, as the hand-made ranking found.
    pub query_evidence: Option<crate::CandidateEvidence>,
    /// In training data: 1 when the row holds an expected answer.
    pub label: u8,
    /// In training data: how well the row meets the search, 0 to 3, as
    /// an LLM graded it.
    pub grade: Option<u8>,
}

impl RowSignals {
    /// The row of a site result and the pages under it.
    pub fn site(hit: &Hit, under: &[&PageHit]) -> Self {
        RowSignals {
            kind: "site".into(),
            key: hit.domain.clone(),
            score: hit.score,
            text_score: hit.text_score,
            placing_text_score: hit.placing_text_score,
            link_score: hit.link_score,
            named: hit.named,
            official: hit.official,
            demand: hit.demand,
            country: hit.country.clone(),
            title: hit.title.clone(),
            description: hit.description.clone(),
            query_evidence: hit.query_evidence.clone(),
            under: under.iter().map(|p| PageSignals::of(p)).collect(),
            ..RowSignals::default()
        }
    }

    /// The row of a page listed on its own.
    pub fn page(hit: &PageHit) -> Self {
        RowSignals {
            kind: "page".into(),
            key: hit.page.url.clone(),
            set: hit.page.set.clone(),
            score: hit.score,
            named: hit.named,
            popularity: hit.popularity,
            whole: hit.whole,
            title: Some(hit.page.title.clone()),
            description: hit.page.description.clone(),
            ..RowSignals::default()
        }
    }

    fn is_page(&self) -> bool {
        self.kind == "page"
    }
}

/// The names of the values [`features`] gives each row, in order.
pub const FEATURES: [&str; 45] = [
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
    "cover_title",
    "cover_text",
    "cover_under",
    "e_cover",
    "e_subst",
    "e_close",
    "e_full_name",
    "e_typed",
    "e_tier",
    "foreign",
    "same_label_above",
    "set_reference",
    "set_subpages",
    "set_docs",
];

/// The query's words that say what it is about, as
/// [`plumb_core::normalize_text`] writes them: all but function and
/// filler words, or all of them when that leaves none.
fn main_words(query: &str) -> Vec<String> {
    let all: Vec<String> = plumb_core::normalize_text(query)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let mut main: Vec<String> = all
        .iter()
        .filter(|w| !plumb_core::is_function_word(w) && !crate::FILLER_WORDS.contains(&w.as_str()))
        .cloned()
        .collect();
    if main.is_empty() {
        main = all;
    }
    main.dedup();
    main
}

/// A word with a plural `s` taken off, so "cicadas" matches "cicada".
fn singular(word: &str) -> &str {
    match word.strip_suffix('s') {
        Some(stem) if stem.chars().count() >= 3 => stem,
        _ => word,
    }
}

/// The share of `words` that `texts` have between them.
fn coverage(words: &[String], texts: &[&str]) -> f64 {
    if words.is_empty() {
        return 0.0;
    }
    let have: std::collections::HashSet<String> = texts
        .iter()
        .flat_map(|t| {
            plumb_core::normalize_text(t)
                .split_whitespace()
                .map(|w| singular(w).to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    let found = words.iter().filter(|w| have.contains(singular(w))).count();
    found as f64 / words.len() as f64
}

/// Whether most letters of `text` are outside ASCII.
fn mostly_foreign(text: &str) -> bool {
    let (mut letters, mut foreign) = (0, 0);
    for ch in text.chars().filter(|c| c.is_alphabetic()) {
        letters += 1;
        foreign += usize::from(!ch.is_ascii());
    }
    letters > 0 && foreign * 2 > letters
}

/// What the model reads of each of `rows`, listed in this order for
/// `query`: one value per name of [`FEATURES`]. Values a row does not
/// have (a page's text match) are 0.
pub fn features(query: &str, rows: &[RowSignals]) -> Vec<[f64; FEATURES.len()]> {
    let nwords = query.split_whitespace().count() as f64;
    let question = f64::from(u8::from(crate::pages::asked_as_question(query)));
    let sites = || rows.iter().filter(|r| !r.is_page());
    let best_score = sites().map(|r| f64::from(r.score)).fold(0.0, f64::max);
    let best_link = sites().map(|r| f64::from(r.link_score)).fold(0.0, f64::max);
    let flag = |b: bool| f64::from(u8::from(b));
    let words = main_words(query);
    let ascii_query = query.is_ascii();
    let label = |row: &RowSignals| plumb_core::domain_label(&row.key);
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let mut f = [0.0; FEATURES.len()];
            let pos = (i + 1) as f64;
            f[0] = 1.0 / pos;
            f[1] = pos;
            f[2] = nwords;
            f[3] = question;
            f[4] = flag(row.is_page());
            if row.is_page() {
                if let Some(set) = SETS.iter().position(|s| row.set.starts_with(s)) {
                    f[5 + set] = 1.0;
                }
                // Sets the first models were not trained with.
                if let Some(set) = LATER_SETS.iter().position(|s| row.set.starts_with(s)) {
                    f[42 + set] = 1.0;
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
                f[33] = under()
                    .map(|p| coverage(&words, &[&p.title]))
                    .fold(0.0, f64::max);
                let evidence = row.query_evidence.as_ref();
                f[34] = evidence.map_or(-1.0, |e| f64::from(e.query_coverage));
                f[35] = evidence.map_or(-1.0, |e| f64::from(e.substantive_coverage));
                f[36] = evidence
                    .and_then(|e| e.semantic_closeness)
                    .map_or(-1.0, f64::from);
                f[37] = flag(evidence.is_some_and(|e| e.full_name));
                f[38] = flag(evidence.is_some_and(|e| e.typed_domain));
                f[39] = evidence.map_or(-1.0, |e| f64::from(e.relevance_tier));
                f[41] = flag(
                    rows[..i]
                        .iter()
                        .any(|above| !above.is_page() && label(above) == label(row)),
                );
            }
            let title = row.title.as_deref().unwrap_or("");
            let description = row.description.as_deref().unwrap_or("");
            // A site's address names it as much as its title does.
            let address = if row.is_page() { "" } else { row.key.as_str() };
            f[31] = coverage(&words, &[title, address]);
            f[32] = coverage(&words, &[title, address, description]);
            f[40] = flag(ascii_query && mostly_foreign(&format!("{title} {description}")));
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

    /// The model in `json` (as `plumb-tools train-rank` writes it). A
    /// model made before features were added reads the ones it knew.
    pub fn from_json(json: &str) -> Result<Self> {
        let model: Model = serde_json::from_str(json).context("reading a ranking model")?;
        if model.features.len() > FEATURES.len()
            || model
                .features
                .iter()
                .zip(FEATURES)
                .any(|(made, now)| made != now)
        {
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

    /// The model that puts a search's first [`LEARNED_SITES`] sites in
    /// order ([`reorder_sites`]), trained on the sites `eval/` lists and
    /// on an LLM's grades of the sites other searches list (`plumb-tools
    /// train-rank --rows 100`).
    pub fn sites() -> &'static Model {
        static MODEL: OnceLock<Model> = OnceLock::new();
        MODEL.get_or_init(|| {
            Model::from_json(include_str!("learned_sites_model.json"))
                .expect("the built-in site ranking model reads")
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
        self.order_first(query, rows, LEARNED_ROWS)
    }

    /// [`Model::order`], with the first `depth` rows moving.
    pub fn order_first(&self, query: &str, rows: &[RowSignals], depth: usize) -> Vec<usize> {
        let top = rows.len().min(depth);
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
/// `waits` says moved down until every row `leads` says that was above it in
/// the hand-made order (a lower row) is listed. The others keep their order.
fn after_leading(
    rows: Vec<usize>,
    waits: impl Fn(usize) -> bool,
    leads: impl Fn(usize) -> bool,
) -> Vec<usize> {
    let leading: Vec<usize> = rows
        .iter()
        .copied()
        .filter(|&row| leads(row) && !waits(row))
        .collect();
    let mut listed = Vec::with_capacity(rows.len());
    let mut waiting = Vec::new();
    for row in rows {
        waiting.push(row);
        while let Some(at) = waiting
            .iter()
            .position(|&row| !waits(row) || leading.iter().all(|s| *s > row || listed.contains(s)))
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
/// A record the query names by its identifier is not read, as the model
/// never saw one, and stays first ([`crate::pages::keep_page_rules`]).
pub fn reorder(model: &Model, query: &str, hits: &mut Vec<Hit>, placed: &mut Vec<PlacedPage>) {
    reorder_moving(model, query, hits, placed, Moves::Rows, None);
}

/// What `cfg` does to the first rows once pages are placed among the
/// sites: [`reorder`] with the built-in model when
/// [`crate::RankConfig::learned`] is on. When the site model has put the
/// sites in order already (`sites_ordered`,
/// [`crate::SearchResults::sites_ordered`]), this model only places the
/// pages listed on their own among them. It learned from the hand-made
/// order, and moving the site model's sites again undid much of its work:
/// in a replay of the test searches, typo searches with their site first
/// fell from 90% to 78%. Otherwise, as without a meaning, it orders the
/// sites too. The page nearest the query in meaning, at the address
/// `meant` ([`crate::pages::PageSearcher::add_meant`]), is not read and
/// stays where placing put it, after [`crate::pages::PARTIAL_AFTER`]
/// sites: the model reads each row's place in the list, so with one
/// listed it moved the sites below it too ("calories in a shot" put
/// davidprotein.com second). Once the rows are in order, model or not, a
/// page only meant comes before the sites just above it that barely match
/// the query (`meant_at`); moved before the model read it, it would move
/// the sites as well.
pub fn reorder_results(
    cfg: &crate::RankConfig,
    query: &str,
    sites_ordered: bool,
    meant: Option<&str>,
    hits: &mut Vec<Hit>,
    placed: &mut Vec<PlacedPage>,
) {
    if cfg.learned {
        let moves = if sites_ordered {
            Moves::Pages
        } else {
            Moves::Rows
        };
        reorder_moving(Model::builtin(), query, hits, placed, moves, meant);
    }
    lift_meant(hits, placed);
}

/// Puts each page listed on its own only for what the query means
/// ([`crate::pages::only_meant`]) before the sites just above it that
/// barely match the query (`meant_at`).
fn lift_meant(hits: &[Hit], placed: &mut [PlacedPage]) {
    for page in placed
        .iter_mut()
        .filter(|page| page.under.is_none() && crate::pages::only_meant(&page.hit))
    {
        page.at = crate::pages::meant_at(hits, page.at);
    }
}

/// What a model moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Moves {
    /// The first [`LEARNED_ROWS`] rows, sites and pages.
    Rows,
    /// The pages among the first [`LEARNED_ROWS`] rows; the sites keep
    /// their order.
    Pages,
    /// The first [`LEARNED_SITES`] sites.
    Sites,
}

fn reorder_moving(
    model: &Model,
    query: &str,
    hits: &mut Vec<Hit>,
    placed: &mut Vec<PlacedPage>,
    moves: Moves,
    meant: Option<&str>,
) {
    let (records, rest): (Vec<PlacedPage>, Vec<PlacedPage>) = std::mem::take(placed)
        .into_iter()
        .partition(|page| crate::pages::is_record(&page.hit));
    let (nearest, rest): (Vec<PlacedPage>, Vec<PlacedPage>) = rest.into_iter().partition(|page| {
        page.under.is_none()
            && crate::pages::only_meant(&page.hit)
            && meant == Some(page.hit.page.url.as_str())
    });
    *placed = rest;
    reorder_rows(model, query, hits, placed, moves);
    // Pages listed on their own stay in the order they are listed, before
    // the pages under sites.
    for mut page in nearest {
        page.hit.learned = Some(learned_place(hits, page.at));
        let at = placed
            .iter()
            .position(|p| p.under.is_some() || p.at > page.at)
            .unwrap_or(placed.len());
        placed.insert(at, page);
    }
    placed.splice(0..0, records);
}

/// Where a page listed on its own before the site at `at` of `hits` is.
fn learned_place(hits: &[Hit], at: usize) -> LearnedPlace {
    match hits.get(at) {
        Some(site) => LearnedPlace::Before(site.domain.clone()),
        None => LearnedPlace::Last,
    }
}

/// Puts the first [`LEARNED_SITES`] of `hits` in the order `model` gives,
/// as [`reorder`] does the first rows once pages are placed, and under
/// the same rules. Deep enough for the model to lift a site the hand-made
/// ranking found but listed far down: "photo sharing app" has
/// instagram.com, with every word and the most links, below the tenth.
pub fn reorder_sites(model: &Model, query: &str, hits: &mut Vec<Hit>) {
    reorder_rows(model, query, hits, &mut Vec::new(), Moves::Sites);
}

/// The site rows of the model's `order`, in the order they fill its site
/// slots under the guards.
fn site_order(
    order: &[usize],
    rows: &[Listed],
    hits: &[Hit],
    placed: &[PlacedPage],
    moves: Moves,
) -> Vec<usize> {
    // The model may reorder within a relevance tier, but an unsupported
    // domain word cannot regain top placement because it is popular.
    // A named page's corroborated official-site identity is also query
    // evidence. Account for it without rewriting the original site-query
    // diagnostics or adding learned input features.
    let page_site = crate::pages::named_page_site(hits, placed.iter().map(|page| &page.hit));
    let mut sites: Vec<_> = order
        .iter()
        .copied()
        .filter(|&row| matches!(rows[row], Listed::Site(_)))
        .collect();
    // Sites another model put in order fill the site slots in that order.
    if moves == Moves::Pages {
        sites.sort_unstable();
    }
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
    let sites = after_leading(sites, bare, |row| !bare(row));
    // Nor a site that lacks some of the query's words past one the whole
    // query names: "denver public library" put denvergov.org, which has
    // only "denver", before denverlibrary.org.
    let site = |row: usize| match rows[row] {
        Listed::Site(i) => Some(&hits[i]),
        Listed::Page(_) => None,
    };
    let partial = |row: usize| site(row).is_some_and(|hit| hit.missing_words);
    let named = |row: usize| site(row).is_some_and(|hit| hit.named);
    let mut sites = after_leading(sites, partial, named);
    // Nor, in the site model's order, past a site that has every word but
    // no vector, as one crawled after the vectors were made: the model
    // reads its missing closeness in meaning as far.
    if moves == Moves::Sites {
        let unembedded = |row: usize| {
            site(row).is_some_and(|hit| {
                !hit.missing_words
                    && hit
                        .query_evidence
                        .as_ref()
                        .is_some_and(|e| e.semantic_closeness.is_none())
            })
        };
        sites = after_leading(sites, partial, unembedded);
        // And a site the query names, whole or among other words by its
        // domain label or an alias, or whose domain it typed, ends no
        // lower than the hand-made ranking put it: trained on graded
        // searches, the model put any-video-converter.com before
        // converter.app for "converter" and capitalone.com before
        // creditonebank.com for "credit one bank official", and
        // `official_site` takes the first site.
        let in_query = |row: usize| {
            site(row).is_some_and(|hit| {
                hit.named
                    || hit
                        .query_evidence
                        .as_ref()
                        .is_some_and(|e| e.typed_domain || e.full_name || e.partial_name_words > 0)
            })
        };
        sites = no_lower(sites, in_query);
    }
    sites
}

/// `order` with each row `keep` holds placed no later than its own index,
/// its place in the listed order (rows there are all sites).
fn no_lower(mut order: Vec<usize>, keep: impl Fn(usize) -> bool) -> Vec<usize> {
    let mut kept = Vec::with_capacity(order.len());
    for at in 0..order.len() {
        let due = order.iter().position(|&row| row <= at && keep(row));
        kept.push(order.remove(due.unwrap_or(0)));
    }
    kept
}

fn reorder_rows(
    model: &Model,
    query: &str,
    hits: &mut Vec<Hit>,
    placed: &mut Vec<PlacedPage>,
    moves: Moves,
) {
    let depth = if moves == Moves::Sites {
        LEARNED_SITES
    } else {
        LEARNED_ROWS
    };
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
    let mut order = model.order_first(query, &signals, depth);
    // Sites and pages keep their model slots; only who fills them changes.
    let mut sites = site_order(&order, &rows, hits, placed, moves).into_iter();
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
    let mut pages = after_leading(pages, inner, |row| !inner(row)).into_iter();
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
        page.hit.learned = Some(learned_place(&new_hits, page.at));
    }
    let under = placed.iter().filter(|p| p.under.is_some()).cloned();
    *placed = alone.into_iter().chain(under).collect();
    *hits = new_hits;
    crate::pages::keep_page_rules(query, hits, placed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::{place_pages, Page, PlacedPage, PARTIAL_AFTER};
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
            meant: false,
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
    fn the_site_model_lifts_a_site_from_far_down_the_list() {
        // The named site is 13th of 120: the model puts it first, sites
        // past the hundredth keep their places, and scores still go down.
        let mut hits: Vec<Hit> = (0..120)
            .map(|i| site(&format!("site{i}.example"), 1.0 - i as f32 / 200.0, i == 12))
            .collect();
        reorder_sites(&named_first_model(), "some site", &mut hits);
        let domains: Vec<&str> = hits.iter().map(|h| h.domain.as_str()).collect();
        assert_eq!(
            domains[..3],
            ["site12.example", "site0.example", "site1.example"]
        );
        assert_eq!(
            domains[LEARNED_SITES],
            format!("site{LEARNED_SITES}.example")
        );
        assert_eq!(hits.len(), 120);
        assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
        // The model nodes use reads.
        assert_eq!(Model::sites().features.len(), FEATURES.len());
    }

    #[test]
    fn after_the_site_model_the_learned_order_only_places_pages() {
        // The model would put the named page first, then swap the named
        // site above the one the site model put first. With the site
        // model on, it only places the page.
        for moves in [Moves::Rows, Moves::Pages] {
            let mut hits = vec![site("a.example", 1.0, false), site("b.example", 0.9, true)];
            let mut placed = vec![PlacedPage {
                hit: article("Some Page", true),
                at: 2,
                under: None,
            }];
            reorder_moving(
                &named_first_model(),
                "some page",
                &mut hits,
                &mut placed,
                moves,
                None,
            );
            let domains: Vec<&str> = hits.iter().map(|h| h.domain.as_str()).collect();
            let expected = if moves == Moves::Rows {
                ["b.example", "a.example"]
            } else {
                ["a.example", "b.example"]
            };
            assert_eq!(domains, expected, "{moves:?}");
            assert_eq!(placed[0].at, 0, "{moves:?}");
        }
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
    fn a_named_article_goes_with_its_own_site_on_a_subdomain() {
        // Wikidata gives Yahoo Mail https://mail.yahoo.com, a site of its
        // own, on yahoo.com: the article goes with mail.yahoo.com, which
        // the query names in full, and does not lift yahoo.com over it.
        let official = |domain: &str, score: f32, named: bool| {
            let mut hit = site(domain, score, named);
            hit.official = true;
            hit.query_evidence = Some(evidence(domain, 0));
            hit
        };
        let mut named = article("Yahoo Mail", true);
        named.page.item = Some("Q1336321".into());
        named.page.site = Some("yahoo.com".into());
        named.page.website = Some("https://mail.yahoo.com".into());
        let mut hits = vec![
            official("mail.yahoo.com", 1.0, true),
            official("yahoo.com", 0.9, false),
        ];
        crate::pages::lift_named_sites(&mut hits, &[named.clone()]);
        let mut placed = place_pages("yahoo mail", &hits, vec![named.clone()]);
        assert_eq!(placed[0].under.as_deref(), Some("mail.yahoo.com"));
        reorder(&tied_model(), "yahoo mail", &mut hits, &mut placed);
        assert_eq!(hits[0].domain, "mail.yahoo.com");
        // Without its own site listed, it goes with yahoo.com.
        let hits = vec![official("yahoo.com", 0.9, false)];
        let placed = place_pages("yahoo mail", &hits, vec![named]);
        assert_eq!(placed[0].under.as_deref(), Some("yahoo.com"));
        // A paper's website is where a free copy is: it goes under no site.
        let mut paper = article("Webmail use in large firms", false);
        paper.page.website = Some("https://mail.yahoo.com/papers/webmail.pdf".into());
        let hits = vec![official("mail.yahoo.com", 1.0, true)];
        let placed = place_pages("yahoo mail", &hits, vec![paper]);
        assert!(placed.iter().all(|p| p.under.is_none()));
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
    fn a_site_with_part_of_the_query_passes_no_site_it_names() {
        let query = "denver public library";
        let order_of = |missing_words: bool, library_score: f32| {
            let mut library = site("denverlibrary.org", library_score, true);
            library.link_score = 0.46;
            let mut city = site("denvergov.org", 0.5, false);
            city.text_score = 0.26;
            city.link_score = 0.6;
            city.missing_words = missing_words;
            let mut hits = vec![library, city, site("other.com", 0.1, false)];
            hits.sort_by(|a, b| b.score.total_cmp(&a.score));
            let mut placed = place_pages(query, &hits, Vec::new());
            reorder(&popular_first_model(), query, &mut hits, &mut placed);
            hits.into_iter().map(|hit| hit.domain).collect::<Vec<_>>()
        };
        // Ranked above it by hand, the library stays above the city's
        // site, which has only "denver".
        assert_eq!(
            order_of(true, 1.1)[..2],
            ["denverlibrary.org", "denvergov.org"]
        );
        // A site with all the words is still the model's to lift, and one
        // ranked above the library by hand keeps its place.
        assert_eq!(order_of(false, 1.1)[0], "denvergov.org");
        assert_eq!(order_of(true, 0.4)[0], "denvergov.org");
    }

    #[test]
    fn the_site_model_lifts_no_site_lacking_words_past_one_with_no_vector() {
        let order_of = |vector: Option<f32>| {
            // schwab.com has every word but, crawled after the vectors
            // were made, maybe no vector; wsj.com is popular and near in
            // meaning but lacks the words. Neither counts as named here,
            // which would keep it in place anyway.
            let mut schwab = site("schwab.com", 1.0, false);
            schwab.link_score = 0.4;
            let mut noted = evidence("schwab.com", 0);
            noted.semantic_closeness = vector;
            noted.partial_name_words = 0;
            schwab.query_evidence = Some(noted);
            let mut wsj = site("wsj.com", 0.8, false);
            wsj.link_score = 0.9;
            wsj.missing_words = true;
            let mut noted = evidence("wsj.com", 0);
            noted.semantic_closeness = Some(1.0);
            noted.partial_name_words = 0;
            wsj.query_evidence = Some(noted);
            let mut hits = vec![schwab, wsj];
            reorder_sites(&popular_first_model(), "charles schwab", &mut hits);
            hits.into_iter().map(|hit| hit.domain).collect::<Vec<_>>()
        };
        assert_eq!(order_of(None), ["schwab.com", "wsj.com"]);
        // With a vector, the model may judge them.
        assert_eq!(order_of(Some(0.2)), ["wsj.com", "schwab.com"]);
    }

    #[test]
    fn the_site_model_puts_a_named_site_no_lower_than_it_was() {
        // `name_words`: the query's words its domain label or an alias
        // covers.
        let linked = |domain: &str, named: bool, name_words: usize, link_score: f32| {
            let mut hit = site(domain, 0.5, named);
            hit.link_score = link_score;
            let mut noted = evidence(domain, 0);
            noted.semantic_closeness = Some(0.8);
            noted.partial_name_words = name_words;
            hit.query_evidence = Some(noted);
            hit
        };
        let order_of = |query: &str, hits: Vec<Hit>| {
            let mut hits = hits;
            reorder_sites(&popular_first_model(), query, &mut hits);
            hits.into_iter().map(|hit| hit.domain).collect::<Vec<_>>()
        };
        // The named site keeps first place from a more linked one.
        let named_first = vec![
            linked("converter.app", true, 0, 0.3),
            linked("any-video-converter.com", false, 0, 0.9),
        ];
        assert_eq!(
            order_of("converter", named_first),
            ["converter.app", "any-video-converter.com"]
        );
        // It may still rise, and the model orders the sites around it.
        let named_last = vec![
            linked("convert.example", false, 0, 0.3),
            linked("converters.example", false, 0, 0.9),
            linked("converter.app", true, 0, 0.9),
        ];
        assert_eq!(
            order_of("converter", named_last),
            ["converters.example", "converter.app", "convert.example"]
        );
        // So does a site whose domain label names it among other words.
        let label_first = vec![
            linked("creditonebank.com", false, 3, 0.3),
            linked("capitalone.com", false, 0, 0.9),
        ];
        assert_eq!(
            order_of("credit one bank official", label_first),
            ["creditonebank.com", "capitalone.com"]
        );
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
            meant: false,
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
    fn a_page_only_meant_by_part_of_the_query_stays_after_three_sites() {
        let feature = |name| FEATURES.iter().position(|f| *f == name).unwrap();
        let pages_first = Model::new(
            vec![Node::Split {
                feature: feature("is_page"),
                threshold: 0.5,
                left: Box::new(Node::Leaf { value: 0.0 }),
                right: Box::new(Node::Leaf { value: 1.0 }),
            }],
            None,
        );
        // Named by another reading of the query too, it is not held.
        for (meant, named) in [(false, false), (true, false), (true, true)] {
            let mut hits: Vec<Hit> = ["a.com", "b.com", "c.com", "d.com"]
                .into_iter()
                .map(|domain| site(domain, 0.5, false))
                .collect();
            let found = PageHit {
                meant,
                ..article("Cons", named)
            };
            let mut placed = place_pages("cons of drinking coffee", &hits, vec![found]);
            reorder(
                &pages_first,
                "cons of drinking coffee",
                &mut hits,
                &mut placed,
            );
            let again = place_pages(
                "cons of drinking coffee",
                &hits,
                placed.iter().map(|p| p.hit.clone()).collect(),
            );
            let at = if meant && !named { PARTIAL_AFTER } else { 0 };
            assert_eq!((placed[0].at, again[0].at), (at, at), "{meant} {named}");
        }
    }

    #[test]
    fn a_page_only_meant_moves_no_site() {
        // Lists the rows past the fourth first: a model reading each row's
        // place moved the sites below a page only meaning found ("calories
        // in a shot" put davidprotein.com second).
        let later_first = Model::new(
            vec![Node::Split {
                feature: FEATURES.iter().position(|f| *f == "pos").unwrap(),
                threshold: 4.5,
                left: Box::new(Node::Leaf { value: 0.0 }),
                right: Box::new(Node::Leaf { value: 1.0 }),
            }],
            None,
        );
        let query = "calories in a shot";
        let five = || -> Vec<Hit> {
            ["a.com", "b.com", "c.com", "d.com", "e.com"]
                .into_iter()
                .enumerate()
                .map(|(i, domain)| site(domain, 0.9 - i as f32 / 10.0, false))
                .collect()
        };
        let order_of = |meant: Option<&str>, pages: Vec<PageHit>| {
            let mut hits = five();
            let mut placed = place_pages(query, &hits, pages);
            reorder_moving(
                &later_first,
                query,
                &mut hits,
                &mut placed,
                Moves::Rows,
                meant,
            );
            let domains: Vec<String> = hits.into_iter().map(|hit| hit.domain).collect();
            (
                domains,
                placed.iter().map(|page| page.at).collect::<Vec<_>>(),
            )
        };
        let shot = PageHit {
            score: 0.95,
            meant: true,
            ..article("Shot glass", false)
        };
        let url = shot.page.url.clone();
        let (sites, _) = order_of(None, Vec::new());
        assert_eq!(sites[0], "e.com");
        assert_eq!(
            order_of(Some(&url), vec![shot.clone()]),
            (sites.clone(), vec![PARTIAL_AFTER])
        );
        // One meant by part of the query is read as before.
        assert_ne!(order_of(None, vec![shot.clone()]).0, sites);
        // Once the model has ordered the sites, it goes before those that
        // barely match the query: a.com and b.com, after e.com there.
        let mut hits = five();
        for hit in &mut hits[..2] {
            hit.missing_words = true;
            hit.text_score = 0.2;
            hit.query_evidence = Some(crate::CandidateEvidence {
                partial_name_words: 0,
                ..evidence(&hit.domain, 1)
            });
        }
        let mut placed = place_pages(query, &hits, vec![shot.clone()]);
        lift_meant(&hits, &mut placed);
        assert_eq!(placed[0].at, PARTIAL_AFTER);
        reorder_moving(
            &later_first,
            query,
            &mut hits,
            &mut placed,
            Moves::Rows,
            Some(&url),
        );
        assert_eq!(placed[0].at, PARTIAL_AFTER);
        lift_meant(&hits, &mut placed);
        assert_eq!((hits[0].domain.as_str(), placed[0].at), ("e.com", 1));
        // It keeps its place among the pages: before one listed later.
        let mut hits = five();
        let later = PlacedPage {
            at: 4,
            under: None,
            hit: article("Shot (album)", false),
        };
        let shot = PlacedPage {
            at: PARTIAL_AFTER,
            under: None,
            hit: shot,
        };
        let mut placed = vec![later, shot];
        let tied = tied_model();
        reorder_moving(
            &tied,
            query,
            &mut hits,
            &mut placed,
            Moves::Rows,
            Some(&url),
        );
        let ats: Vec<usize> = placed.iter().map(|page| page.at).collect();
        assert_eq!(ats, [PARTIAL_AFTER, 4]);
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
            meant: false,
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
            meant: false,
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
            meant: false,
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
