//! The local search index: a Tantivy index over site records, ranked for
//! navigational queries ("us bank" should put usbank.com first).
//!
//! Ranking blends how well the query matches the site's names (BM25 over the
//! domain label, aliases, title, link text and description, plus a "joined"
//! match so "us bank" finds the label `usbank`) with the site's popularity
//! prior, [`plumb_core::link_score`]:
//!
//! `score = alpha * link_score + trust * ((1 - alpha) * text_score + name_bonus) + country`
//!
//! - `alpha` is [`RankConfig::described_alpha`] instead, when set, for a
//!   query no site is named by in full and that names no kind of thing.
//! - `text_score` is the BM25 score normalized to `0..=1` within the
//!   candidates of each query.
//! - `name_bonus` rewards a site whose name the query starts with. A domain
//!   label equal to the first `k` of the query's `n` words gets `k / n` of
//!   [`RankConfig::exact_label_bonus`] (all of it when it is the whole
//!   query, `us bank` -> usbank.com); an alias likewise gets `k / n` of
//!   [`RankConfig::exact_alias_bonus`]. So in "irs refund" irs.gov gets half
//!   the label bonus. A query that is a hostname or URL (`usbank.com`,
//!   `https://www.usbank.com/`) counts as a whole-query label match for
//!   that domain. An official website's Wikidata names count as labels,
//!   with or without a leading "The", so "wall street journal" names
//!   wsj.com as strongly as wall.org names itself.
//!   A site named by the whole query (by its label, an official name or a
//!   typed hostname) also gets a full text match, so popularity decides
//!   among the sites a query names: aa.com, officially "American Airlines",
//!   beats americanairlines.com.
//! - A query that is a kind of thing ("banks", "airlines") gives every site
//!   of that kind ([`SiteRecord::kinds`]) a full text match and
//!   [`RankConfig::kind_bonus`], so they are listed by popularity.
//! - `country` is [`RankConfig::country_boost`] for a site of the searcher's
//!   home country ([`SearchOptions::country`]), minus that for a site of
//!   another country, and 0 for global sites. [`SearchOptions::only_country`]
//!   drops other countries' sites instead.
//! - `trust` guards brand-plus-intent queries ("us bank login") against
//!   look-alikes such as usbank-login-help.com, which stuff every query word
//!   into their titles and domains but have no popularity to show for it.
//!   When the query is a site's name followed by more words, every site
//!   needs a link score of [`RankConfig::trusted_link_score`] (or the best
//!   such named site's, if lower) for its text match and name bonus to
//!   count in full; with less, `trust` falls linearly to
//!   [`RankConfig::untrusted_share`] at a link score of 0. Otherwise `trust`
//!   is 1: a query that is just a name ("us bank") is won by the named site
//!   anyway, and when no site is named a little-known site still comes
//!   first. A typed hostname always has full trust.
//! - A query ending in words that say what someone wants from a site
//!   rather than which site ("login", "docs", "tracking": `INTENT_WORDS`)
//!   is also ranked by the words before them, and a well-known site they
//!   name keeps the better of its two scores, so "paypal login" finds
//!   paypal.com rather than paypal-login.us.
//!   A well-known site ([`WELL_KNOWN_LINK_SCORE`]) named by the whole
//!   query keeps it to itself: "read the docs".
//!
//! A query that names no site in full is checked for typos. It is always
//! searched as typed; a correction is only suggested: "amazom" asks "Did
//! you mean amazon?". See [`Searcher::search_meaning`] and the [`spell`]
//! module.
//!
//! All text, at index and at query time, goes through
//! [`plumb_core::normalize_text`] and is then ASCII-folded, so `U.S. Bank`,
//! `us bank` and `US BANK` are the same query and `nestle` finds `Nestlé`.

mod analysis;
pub mod pages;
pub mod places;
mod replace;
mod schema;
mod spell;

use std::borrow::Borrow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::{
    canonical_domain, kind_key, language_code, normalize_country, normalize_text, other_number,
    registrable_domain, search_link, search_template_for, truncate_chars, AdultLevel, KeyPage,
    Operators, SafeSearch, SiteRecord, MAX_TEXT_CHARS,
};
use serde::{Deserialize, Serialize};
use tantivy::collector::{DocSetCollector, TopDocs};
use tantivy::merge_policy::NoMergePolicy;
use tantivy::query::{BooleanQuery, BoostQuery, EnableScoring, Occur, Query, Scorer, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Value};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{
    DocAddress, DocSet, Index, IndexReader, IndexWriter, Order, ReloadPolicy, TantivyDocument, Term,
};

use crate::replace::Staging;
use crate::schema::Fields;

/// BM25 boost of a query word matching the domain label.
const LABEL_BOOST: f32 = 4.0;
/// BM25 boost of a query word matching a whole joined name.
const JOINED_BOOST: f32 = 4.0;
/// BM25 boost of a query word matching an alias.
const ALIASES_BOOST: f32 = 2.5;
/// BM25 boost of a query word matching the title.
const TITLE_BOOST: f32 = 2.0;
/// BM25 boost of a query word matching inbound link text.
const ANCHORS_BOOST: f32 = 1.5;
/// BM25 boost of a query word matching the description.
const DESCRIPTION_BOOST: f32 = 0.5;
/// BM25 boost of a query word matching Wikidata's description of the
/// organization. Higher than the site's own description: Wikidata's is
/// written by others, so look-alikes cannot stuff it with search words.
const ABOUT_BOOST: f32 = 2.0;
/// BM25 boost of a query word matching a homepage heading.
const HEADINGS_BOOST: f32 = 0.5;
/// BM25 boost of the whole query, joined (`us bank` -> `usbank`), matching a
/// joined name or a label word.
const WHOLE_QUERY_BOOST: f32 = 6.0;

/// Small words that join the words of a longer query ("pizza in denver",
/// "bank of america") and name no site there: between two other words they
/// never match a domain label or joined name on their own, so in.gov is not
/// found by them, and one alone never counts as a leading name ("in n out
/// burger" does not name in.gov). A one-word query is still a name.
const FUNCTION_WORDS: &[&str] = &[
    "a", "an", "and", "at", "by", "for", "from", "in", "into", "near", "of", "on", "or", "the",
    "to", "with",
];

fn is_function_word(word: &str) -> bool {
    FUNCTION_WORDS.contains(&word)
}
/// BM25 boost of a query that is the hostname or URL of an indexed domain.
const DOMAIN_BOOST: f32 = 10.0;
/// The share of a word's boost its other number gets ("video" for
/// "videos").
const OTHER_NUMBER_SHARE: f32 = 0.8;
/// The most popular sites matching any query word that are ranked even if
/// BM25 put them below [`RankConfig::candidates`] others: a query that
/// describes a big site ("watch videos online") matches many small ones
/// that repeat its words.
const POPULAR_CANDIDATES: usize = 50;
/// The sites nearest a query in meaning ([`Meaning::nearest`]) that are
/// ranked, nearest first.
const NEAREST_RANKED: usize = 50;
/// The most popular of the other sites [`Meaning::nearest`] gives that are
/// ranked too.
const NEAREST_POPULAR: usize = 50;
/// Most distinct query words used; the rest are ignored.
const MAX_QUERY_WORDS: usize = 16;
/// A query with operators ranks this many times its limit, and at least
/// [`OPERATOR_CANDIDATES`], before they narrow the hits.
const OPERATOR_WIDENING: usize = 5;
const OPERATOR_CANDIDATES: usize = 200;
/// The least link score of a well-known site (roughly the top 30,000).
pub const WELL_KNOWN_LINK_SCORE: f32 = 0.5;
/// How much more link score a well-known site whose name is a typo away
/// must have than what a query finds as typed for the query to be taken as
/// that typo: twitter.com over twiter.com.
pub const TYPO_POPULARITY_MARGIN: f32 = 0.3;
/// A site the whole query names with this link score or more keeps its
/// name even when a far better-known one is a typo away. Typo-squatters
/// such as twiter.com score below it (0.25 to 0.37 on real data).
pub const KEEPS_ITS_NAME_LINK_SCORE: f32 = 0.4;
/// Words that say what someone wants from a site rather than which site:
/// "paypal login", "postgres docs", "usps tracking". Look-alikes put them
/// in their domains (paypal-login.us), and big hosts match them in their
/// link text (github.com for "docs"), so a query ending in them is ranked
/// by the words before them. Each entry is one or more normalized words.
const INTENT_WORDS: &[&str] = &[
    "login",
    "log in",
    "logon",
    "log on",
    "signin",
    "sign in",
    "sign on",
    "account",
    "my account",
    "support",
    "help",
    "help center",
    "customer service",
    "contact",
    "docs",
    "web docs",
    "documentation",
    "official site",
    "official website",
    "website",
    "homepage",
    "home page",
    "download",
    "portal",
    "tracking",
    "check in",
    "careers",
    "investor relations",
];
/// Memory budget of the index writer, shared by its threads. Enough for a
/// million records without flushing tiny segments.
const WRITER_HEAP_BYTES: usize = 200_000_000;

/// Ranking knobs. Missing fields deserialize to their [`Default`] values.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RankConfig {
    /// Weight of the popularity prior; the text match gets `1 - alpha`.
    pub alpha: f32,
    /// How many BM25 candidates are re-ranked per query. Sites whose name
    /// the query starts with are re-ranked too.
    pub candidates: usize,
    /// Added when the query, joined, equals the domain label (`us bank` ->
    /// `usbank`). A label equal to the first `k` of the query's `n` words
    /// gets `k / n` of it (`irs` in "irs refund": half).
    pub exact_label_bonus: f32,
    /// Added instead when the query, joined, equals one of the site's
    /// aliases (`ally bank` for ally.com), and `k / n` of it for an alias
    /// equal to the first `k` words. Smaller than the label bonus because
    /// sites can pick their own aliases (`og:site_name`).
    pub exact_alias_bonus: f32,
    /// When the query is a site's name (its domain label or an alias)
    /// followed by more words, `irs` + `refund`, every site needs this link
    /// score, or the named site's if that is lower, for its text match and
    /// name bonus to count in full. Keeps look-alikes that stuff every query
    /// word into their titles below the site they imitate. 0 turns this off.
    pub trusted_link_score: f32,
    /// The share of its text match and name bonus that a site with a link
    /// score of 0 keeps in that case. It grows linearly to all of it at
    /// [`RankConfig::trusted_link_score`].
    pub untrusted_share: f32,
    /// Added when the whole query names what kind of thing a site is
    /// ("banks" for a site whose Wikidata kind is "bank"), whose text match
    /// then counts as full, so the sites of that kind come first, most
    /// popular first.
    pub kind_bonus: f32,
    /// With a home country ([`SearchOptions::country`]), added for sites of
    /// that country and taken off sites of any other country. Sites that
    /// belong to no country (most `.com`s) are left alone.
    pub country_boost: f32,
    /// With a [`Meaning`] and a query that no site is named by in full,
    /// the share of the text match that comes from how close each site is
    /// in meaning; the words matched give the rest. A site with no
    /// embedding is taken to be as close as its words match, times the
    /// share of the query's words it has.
    pub meaning_weight: f32,
    /// [`RankConfig::alpha`] for a query that describes what it looks for:
    /// no site is named by all of it and it names no kind of thing
    /// ("code hosting"). Small sites that repeat such a query's words match
    /// it better than the big site it describes. `None` keeps `alpha`.
    pub described_alpha: Option<f32>,
    /// [`RankConfig::exact_label_bonus`] for a domain label equal to only
    /// the first `k` of the query's `n` words, which then gets `k / n` of
    /// it (code.gov in "code hosting"). `None` keeps the full-name bonus.
    pub partial_label_bonus: Option<f32>,
    /// For a query that describes what it looks for, the text match (words
    /// and meaning together) a site needs for its popularity to count in
    /// full; below it, popularity counts in proportion. Keeps the most
    /// popular sites, which match "to do list" or "map of the world" not
    /// at all (under 0.01 on a million sites), from outranking every site
    /// that does, while youtube.com ("video sharing site") and spotify.com
    /// ("music streaming"), at about 0.04, keep theirs. `None` turns it off.
    pub described_relevance: Option<f32>,
}

impl Default for RankConfig {
    fn default() -> Self {
        RankConfig {
            alpha: 0.35,
            candidates: 200,
            exact_label_bonus: 0.25,
            exact_alias_bonus: 0.1,
            trusted_link_score: 0.2,
            untrusted_share: 0.5,
            kind_bonus: 0.25,
            country_boost: 0.06,
            meaning_weight: 0.7,
            described_alpha: Some(0.5),
            partial_label_bonus: None,
            described_relevance: Some(0.04),
        }
    }
}

/// How close in meaning a query is to sites, from embeddings of the query
/// and of each site's text: for queries that describe what they look for
/// ("electric car maker") rather than name it.
pub trait Meaning {
    /// Domains of the sites nearest the query in meaning, nearest first.
    /// The first 50 are ranked, and the 50 most popular of the rest.
    fn nearest(&self) -> Vec<String>;
    /// How close the site of `domain` is to the query, in `0..=1`; `None`
    /// for a site with no embedding.
    fn closeness(&self, domain: &str) -> Option<f32>;
}

/// What [`build_index`] built. Every record is a document, merged into
/// another one, skipped or folded into the site it redirects to: `docs +
/// merged + skipped + redirected` is the number of records.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStats {
    /// Documents in the index, one per domain.
    pub docs: u64,
    /// Records merged into an earlier record for the same domain.
    #[serde(default)]
    pub merged: u64,
    /// Records skipped because their domain is not a valid registrable
    /// domain ([`plumb_core::canonical_domain`] rejects it), empty ones
    /// included.
    #[serde(default)]
    pub skipped: u64,
    /// Records left out because their homepage redirects to another site in
    /// the index, whose names they joined.
    #[serde(default)]
    pub redirected: u64,
}

/// One search result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub domain: String,
    /// The record's url, or `https://<domain>/` when it has none.
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// Final blended score.
    pub score: f32,
    /// Normalized text match in `0..=1`, before the name bonus and trust.
    pub text_score: f32,
    /// [`plumb_core::link_score`] of the site.
    pub link_score: f32,
    /// The country the site belongs to ([`plumb_core::site_country`]),
    /// `None` for global sites.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// The whole query is the site's name (its label or an official name)
    /// or its hostname.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub named: bool,
    /// The site is the official website of something Wikidata describes
    /// (its index entry has Wikidata's description of it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub official: bool,
    /// The site's key pages ("sitelinks": sign in, docs, pricing), for
    /// listing under it when it is the site searched for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_pages: Vec<KeyPage>,
}

/// Per-search choices of the person searching.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchOptions {
    /// Home country, an ISO 3166-1 alpha-2 code (`US`): its sites get
    /// [`RankConfig::country_boost`] and other countries' sites lose it.
    pub country: Option<String>,
    /// Leave out sites of countries other than [`SearchOptions::country`].
    /// Global sites stay. Does nothing without a country.
    pub only_country: bool,
    /// Search for the query exactly as typed, without correcting typos.
    pub exact: bool,
    /// What safe search leaves out; see [`plumb_core::safe`].
    pub safe: SafeSearch,
    /// Leave out sites whose homepage is in another language than this
    /// one (a language code, `en`). Sites that do not say stay.
    pub language: Option<String>,
    /// How the results page shows recent headlines; the index ignores it.
    pub recent: plumb_core::RecentNews,
}

/// A link into a site's own search for the words after its name:
/// "github plumb search" -> search github.com for "plumb search".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteSearch {
    pub domain: String,
    /// The words searched for, as typed.
    pub terms: String,
    /// The site's search address for them.
    pub url: String,
}

/// What [`Searcher::search_full`] finds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchResults {
    pub hits: Vec<Hit>,
    /// Single pages (Wikipedia articles) listed with the sites; see
    /// [`pages::place_pages`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<pages::PlacedPage>,
    /// Offered when the query starts with a site's name and goes on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_search: Option<SiteSearch>,
    /// Set when the query looks misspelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spelling: Option<Spelling>,
}

/// A suggested spelling of a query: "amazom" -> "amazon".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spelling {
    /// The corrected query, in lowercase words without punctuation. The
    /// hits are always for the query as typed; this is only a suggestion
    /// ("Did you mean ...?").
    pub query: String,
}

/// Builds a fresh index of `records` in `dir`, replacing any index already
/// there.
///
/// Each record's domain is made canonical with
/// [`plumb_core::canonical_domain`] (`Example.com` -> `example.com`,
/// `münchen.de` -> `xn--mnchen-3ya.de`, `www.example.com` ->
/// `example.com`), and records that end up with the same domain are merged
/// into one document with [`SiteRecord::merge`], as
/// [`plumb_core::RecordSet`] would. Records whose domain is not a valid
/// registrable domain are skipped. The returned [`IndexStats`] counts both.
///
/// The index is built in a hidden directory next to `dir` and swapped in
/// only once it is complete, so a failed build leaves the previous index in
/// place and removes its own leftovers. A `Searcher` opened before keeps
/// serving the previous index (on Unix; elsewhere the swap may fail while
/// it is open).
///
/// Replacing deletes everything in `dir`, so to protect other data `dir`
/// must be missing, empty or an index: every index built here holds a
/// marker file, `.plumb-index`, and a directory without one is replaced
/// only when it holds a Tantivy index and nothing else (as indexes built
/// before the marker do). Anything else is refused with an error naming
/// `dir`. If `dir` is a symlink, the directory it points to is replaced.
/// Its parent must be writable, and `dir` itself cannot be a mount point
/// (mount the parent).
///
/// `records` may be the records themselves or references to them, so a
/// caller holding a [`plumb_core::RecordSet`] need not copy it into a list.
pub fn build_index<R: Borrow<SiteRecord>>(dir: &Path, records: &[R]) -> Result<IndexStats> {
    let (sites, mut stats) = merge_by_domain(records);
    let (sites, redirect_names) = fold_redirects(sites);
    stats.redirected = stats.docs - sites.len() as u64;
    stats.docs = sites.len() as u64;
    let staging = Staging::new(dir)?;
    write_index(staging.path(), &sites, &redirect_names)?;
    staging.install()?;
    Ok(stats)
}

/// A site to index: one of the records given, or a copy where records had
/// to be merged or their domain changed. Two words, where a
/// `Cow<SiteRecord>` would take the size of a whole record for each of a
/// million sites even when it only borrows.
enum Site<'a> {
    Borrowed(&'a SiteRecord),
    Owned(Box<SiteRecord>),
}

impl Site<'_> {
    fn to_mut(&mut self) -> &mut SiteRecord {
        if let Site::Borrowed(record) = *self {
            *self = Site::Owned(Box::new(record.clone()));
        }
        match self {
            Site::Owned(record) => record,
            Site::Borrowed(_) => unreachable!("made owned above"),
        }
    }
}

impl std::ops::Deref for Site<'_> {
    type Target = SiteRecord;

    fn deref(&self) -> &SiteRecord {
        match self {
            Site::Borrowed(record) => record,
            Site::Owned(record) => record,
        }
    }
}

/// One record per canonical domain ([`canonical_domain`]), in the order the
/// domains first appear: records for the same domain are merged
/// ([`SiteRecord::merge`]) and records without a valid domain are skipped.
/// Records that are already canonical and unique are not copied.
fn merge_by_domain<R: Borrow<SiteRecord>>(records: &[R]) -> (Vec<Site<'_>>, IndexStats) {
    let mut sites: Vec<Site> = Vec::with_capacity(records.len());
    let mut positions: HashMap<String, usize> = HashMap::with_capacity(records.len());
    let mut stats = IndexStats::default();
    for record in records {
        let record: &SiteRecord = record.borrow();
        let Some(domain) = canonical_domain(&record.domain) else {
            stats.skipped += 1;
            continue;
        };
        let with_domain = |domain: &String| {
            let mut record = record.clone();
            record.domain.clone_from(domain);
            record
        };
        match positions.entry(domain) {
            Entry::Occupied(entry) => {
                let other = with_domain(entry.key());
                sites[*entry.get()].to_mut().merge(other);
                stats.merged += 1;
            }
            Entry::Vacant(entry) => {
                let site = if record.domain == *entry.key() {
                    Site::Borrowed(record)
                } else {
                    Site::Owned(Box::new(with_domain(entry.key())))
                };
                entry.insert(sites.len());
                sites.push(site);
            }
        }
    }
    stats.docs = sites.len() as u64;
    (sites, stats)
}

/// Leaves out the sites whose homepage redirects to another site in
/// `sites` (`pncbank.com` -> `pnc.com`): they are that site under another
/// name. Their domain labels become names of the site they redirect to,
/// returned by its position in the sites kept, so "pnc bank" names pnc.com.
fn fold_redirects(sites: Vec<Site<'_>>) -> (Vec<Site<'_>>, HashMap<usize, Vec<String>>) {
    let positions: HashMap<&str, usize> = sites
        .iter()
        .enumerate()
        .map(|(i, site)| (site.domain.as_str(), i))
        .collect();
    // Only one step: a site that itself redirects is not a target.
    let target_of = |site: &SiteRecord| {
        let to = canonical_domain(&site.redirect.as_ref()?.to)?;
        let &target = positions.get(to.as_str())?;
        (sites[target].redirect.is_none() && sites[target].domain != site.domain).then_some(target)
    };
    let targets: Vec<Option<usize>> = sites.iter().map(|site| target_of(site)).collect();
    let mut kept_at = vec![None; sites.len()];
    let mut kept = 0;
    for (i, target) in targets.iter().enumerate() {
        if target.is_none() {
            kept_at[i] = Some(kept);
            kept += 1;
        }
    }
    let mut names: HashMap<usize, Vec<String>> = HashMap::new();
    for (i, target) in targets.iter().enumerate() {
        if let Some(target) = target.and_then(|t| kept_at[t]) {
            names
                .entry(target)
                .or_default()
                .push(schema::label_text(&sites[i].domain));
        }
    }
    drop(positions);
    let sites = sites
        .into_iter()
        .zip(&targets)
        .filter(|(_, target)| target.is_none())
        .map(|(site, _)| site)
        .collect();
    (sites, names)
}

/// Writes a complete index of `sites`, whose domains are canonical and
/// unique, into the empty directory `dir`: one commit, then a merge into a
/// single segment. `redirect_names` holds the names other sites give each
/// site by redirecting to it, by position.
fn write_index(
    dir: &Path,
    sites: &[Site],
    redirect_names: &HashMap<usize, Vec<String>>,
) -> Result<()> {
    let schema = schema::schema();
    let fields = Fields::new(&schema)?;
    let index = Index::create_in_dir(dir, schema)
        .with_context(|| format!("creating index in {}", dir.display()))?;
    analysis::register(index.tokenizers());

    let mut writer: IndexWriter = index
        .writer(WRITER_HEAP_BYTES)
        .context("opening index writer")?;
    // Merge once at the end instead of while indexing.
    writer.set_merge_policy(Box::new(NoMergePolicy));
    for (i, site) in sites.iter().enumerate() {
        let names = redirect_names.get(&i).map_or(&[][..], Vec::as_slice);
        writer.add_document(schema::document(&fields, site, names))?;
    }
    writer.commit().context("committing index")?;
    fail_point("after_commit")?;

    // The index is read-only from here on, and one segment searches fastest.
    let segments = index.searchable_segment_ids()?;
    if segments.len() > 1 {
        writer
            .merge(&segments)
            .wait()
            .context("merging index segments")?;
    }
    writer
        .wait_merging_threads()
        .context("finishing index merges")?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// The step at which [`fail_point`] makes the build fail on this thread.
    static FAIL_AT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

/// Fails at `step` when a test asked for it, so tests can break a build
/// halfway. Does nothing outside tests.
#[cfg(test)]
fn fail_point(step: &str) -> Result<()> {
    if FAIL_AT.get() == Some(step) {
        bail!("injected failure at {step}");
    }
    Ok(())
}

#[cfg(not(test))]
fn fail_point(_step: &str) -> Result<()> {
    Ok(())
}

/// A read-only handle on an index built by [`build_index`]. Cheap to share
/// between threads (`Send + Sync`), e.g. behind an `Arc` in a web server.
///
/// It searches the index as it was when opened: after rebuilding the index,
/// open a new `Searcher`.
pub struct Searcher {
    reader: IndexReader,
    fields: Fields,
    words: TextAnalyzer,
    joined: TextAnalyzer,
}

impl Searcher {
    /// Opens the index in `dir`. Fails when there is none, or when it was
    /// built with a different schema (rebuild it then).
    pub fn open(dir: &Path) -> Result<Self> {
        let index = Index::open_in_dir(dir)
            .with_context(|| format!("opening search index in {}", dir.display()))?;
        analysis::register(index.tokenizers());
        if index.schema() != schema::schema() {
            bail!(
                "the index in {} was built by another version of plumb-index; rebuild it",
                dir.display()
            );
        }
        let fields = Fields::new(&index.schema())?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .with_context(|| format!("reading search index in {}", dir.display()))?;
        Ok(Searcher {
            reader,
            fields,
            words: analysis::words_analyzer(),
            joined: analysis::joined_analyzer(),
        })
    }

    /// Number of documents (sites) in the index.
    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Whether the index holds `domain`, a canonical domain.
    pub fn has_domain(&self, domain: &str) -> bool {
        let term = Term::from_field_text(self.fields.domain, domain);
        self.reader.searcher().doc_freq(&term).is_ok_and(|n| n > 0)
    }

    /// [`Searcher::search_with`] using [`RankConfig::default`].
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.search_with(query, limit, &RankConfig::default())
    }

    /// Best `limit` hits for `query`, best first, with no home country. See
    /// [`Searcher::search_full`].
    pub fn search_with(&self, query: &str, limit: usize, cfg: &RankConfig) -> Result<Vec<Hit>> {
        Ok(self
            .search_full(query, limit, cfg, &SearchOptions::default())?
            .hits)
    }

    /// Best `limit` hits for `query`, best first, and a link into a named
    /// site's own search when the query goes on after the site's name. A
    /// query with no letters or digits finds nothing.
    ///
    /// The top `max(cfg.candidates, limit)` documents by BM25, plus every
    /// site whose name the query starts with and every site of the kind
    /// the query names, are re-ranked by the blended score (see the
    /// [crate docs](crate)); ties go to the higher link score, then to the
    /// alphabetically first domain.
    pub fn search_full(
        &self,
        query_text: &str,
        limit: usize,
        cfg: &RankConfig,
        options: &SearchOptions,
    ) -> Result<SearchResults> {
        self.search_meaning(query_text, limit, cfg, options, None)
    }

    /// [`Searcher::search_full`], also ranking by `meaning` when no site is
    /// named by the whole query and the query names no kind of site: the
    /// sites [`Meaning::nearest`] the query are re-ranked too, and
    /// [`RankConfig::meaning_weight`] of every text match is how close the
    /// site is in meaning. Queries that name a site rank as without it.
    ///
    /// The results are always for the query as typed. Typos get a suggested
    /// correction ([`SearchResults::spelling`], see [`spell`]) unless
    /// [`SearchOptions::exact`] is set or the query is a hostname. A query
    /// that names a site in full gets one only when its first words are a
    /// typo away from the name of a well-known site
    /// ([`WELL_KNOWN_LINK_SCORE`]) with [`TYPO_POPULARITY_MARGIN`] more
    /// link score than what the query finds as typed (the sites it names
    /// in full, if below [`KEEPS_ITS_NAME_LINK_SCORE`], else its best hit):
    /// "twiter" lists the typo-squatter twiter.com and asks "Did you mean
    /// twitter?". Any other query gets one when the correction finds
    /// something.
    ///
    /// Search operators ([`Operators`]) narrow the results: `site:`
    /// keeps the sites on that host and lists the site itself after them,
    /// with a link into its own search; `"quotes"` keep the sites whose
    /// name, title or description has the words in that order; `-word`
    /// leaves out the sites that have the word. Queries with operators are
    /// not corrected for typos.
    pub fn search_meaning(
        &self,
        query_text: &str,
        limit: usize,
        cfg: &RankConfig,
        options: &SearchOptions,
        meaning: Option<&dyn Meaning>,
    ) -> Result<SearchResults> {
        let ops = Operators::parse(query_text);
        if !ops.any() {
            return self.search_words(query_text, limit, cfg, options, meaning);
        }
        let mut results = SearchResults::default();
        if limit == 0 {
            return Ok(results);
        }
        let options = SearchOptions {
            exact: true,
            ..options.clone()
        };
        if !ops.words.is_empty() {
            let wider = limit
                .saturating_mul(OPERATOR_WIDENING)
                .max(OPERATOR_CANDIDATES);
            let found = self.search_words(&ops.words, wider, cfg, &options, meaning)?;
            results.hits = found
                .hits
                .into_iter()
                .filter(|hit| ops.allows(&hit.domain, hit_texts(hit)))
                .collect();
            results.site_search = found
                .site_search
                .filter(|link| ops.allows_host(&link.domain));
        }
        // The sites `site:` names come after what matched in them, even
        // when their homepage does not have the words: the link into their
        // own search finds the rest.
        let fallback = Operators {
            phrases: Vec::new(),
            ..ops.clone()
        };
        for site in &ops.sites {
            let Some(domain) = registrable_domain(site) else {
                continue;
            };
            if !results.hits.iter().any(|hit| hit.domain == domain) {
                let found = self.search_words(&domain, 1, cfg, &options, None)?;
                results.hits.extend(found.hits.into_iter().filter(|hit| {
                    hit.domain == domain && fallback.allows(&hit.domain, hit_texts(hit))
                }));
            }
            if results.site_search.is_none() {
                results.site_search = self.site_search_of(&domain, &ops.site_terms)?;
            }
        }
        results.hits.truncate(limit);
        Ok(results)
    }

    /// [`Searcher::search_meaning`] for a query without operators.
    fn search_words(
        &self,
        query_text: &str,
        limit: usize,
        cfg: &RankConfig,
        options: &SearchOptions,
        meaning: Option<&dyn Meaning>,
    ) -> Result<SearchResults> {
        let (results, named) = self.rank(query_text, limit, cfg, options, meaning)?;
        if options.exact || limit == 0 || named.typed {
            return Ok(results);
        }
        // "paypal login" is also ranked as "paypal": a well-known site that
        // name alone names keeps the better of its two scores, so
        // paypal.com goes above paypal-login.us. Lesser sites the name
        // names keep the whole query's score: postgres.ai is not what
        // "postgres docs" is after. A well-known site named by all of it
        // keeps the query to itself (readthedocs.org for "read the docs").
        // The link into the named site's own search still uses every word.
        if let Some(name) = without_intent_words(query_text) {
            let named_in_full = named
                .full_link_score
                .is_some_and(|score| score >= WELL_KNOWN_LINK_SCORE);
            if !named_in_full {
                let mut found = self.search_words(&name, limit, cfg, options, meaning)?;
                let mut best: HashMap<String, Hit> = HashMap::new();
                let by_name = std::mem::take(&mut found.hits)
                    .into_iter()
                    .filter(|hit| hit.named && hit.link_score >= WELL_KNOWN_LINK_SCORE);
                for hit in results.hits.into_iter().chain(by_name) {
                    match best.entry(hit.domain.clone()) {
                        Entry::Occupied(mut kept) if kept.get().score < hit.score => {
                            kept.insert(hit);
                        }
                        Entry::Occupied(_) => {}
                        Entry::Vacant(slot) => {
                            slot.insert(hit);
                        }
                    }
                }
                let mut hits: Vec<Hit> = best.into_values().collect();
                hits.sort_by(|a, b| {
                    b.score
                        .total_cmp(&a.score)
                        .then_with(|| b.link_score.total_cmp(&a.link_score))
                        .then_with(|| a.domain.cmp(&b.domain))
                });
                hits.truncate(limit);
                found.hits = hits;
                found.site_search = results.site_search.or(found.site_search);
                // A suggested spelling of the name keeps the words after it:
                // "postgres docs" suggests "postgresql docs".
                if let Some(spelling) = &mut found.spelling {
                    let words = normalize_text(query_text);
                    let after = words
                        .split_whitespace()
                        .skip(name.split_whitespace().count());
                    for word in after {
                        spelling.query.push(' ');
                        spelling.query.push_str(word);
                    }
                }
                return Ok(found);
            }
        }
        // A site named in full with popularity of its own keeps its name;
        // so does a kind of site.
        match named.full_link_score {
            Some(score) if score >= KEEPS_ITS_NAME_LINK_SCORE => return Ok(results),
            None if named.kind => return Ok(results),
            _ => {}
        }
        let searcher = self.reader.searcher();
        let link_score = |docs: &HashSet<DocAddress>| best_link_score(&searcher, docs);
        let named_words = if named.full_link_score.is_some() {
            0
        } else {
            named.words
        };
        let Some(fix) = spell::correct(
            &searcher,
            &self.fields,
            &self.words,
            query_text,
            named_words,
            &link_score,
        )?
        else {
            return Ok(results);
        };
        let typed_link_score = named
            .full_link_score
            .or_else(|| results.hits.first().map(|hit| hit.link_score))
            .unwrap_or(0.0);
        // A site the whole query names is not taken for a typo of a name
        // that only some of its words are a typo of: "linus tech tips" is
        // linustechtips.com, not "linux" with two more words.
        let fixes_the_named_name =
            named.full_link_score.is_none() || fix.name_covers >= named.words;
        let far_more_popular = fixes_the_named_name
            && fix.name_link_score.is_some_and(|score| {
                score >= WELL_KNOWN_LINK_SCORE && score >= typed_link_score + TYPO_POPULARITY_MARGIN
            });
        if named.full_link_score.is_some() && !far_more_popular {
            return Ok(results);
        }
        // The results stay those of the query as typed; the correction is
        // only offered ("Did you mean ...?"), and only when it finds
        // something.
        let (fixed, _) = self.rank(&fix.query, 1, cfg, options, meaning)?;
        if fixed.hits.is_empty() {
            return Ok(results);
        }
        Ok(SearchResults {
            spelling: Some(Spelling { query: fix.query }),
            ..results
        })
    }

    /// The ranked results of `query_text` as typed, and how the query names
    /// sites.
    fn rank(
        &self,
        query_text: &str,
        limit: usize,
        cfg: &RankConfig,
        options: &SearchOptions,
        meaning: Option<&dyn Meaning>,
    ) -> Result<(SearchResults, Named)> {
        if limit == 0 {
            return Ok(Default::default());
        }
        let Some(query) = ParsedQuery::new(query_text, &self.words, &self.joined) else {
            return Ok(Default::default());
        };
        let searcher = self.reader.searcher();
        let num_docs = usize::try_from(searcher.num_docs()).unwrap_or(usize::MAX);
        if num_docs == 0 {
            return Ok(Default::default());
        }

        let text_query = query.text_query(&searcher, &self.fields)?;
        let num_candidates = cfg.candidates.max(limit).min(num_docs);
        let mut candidates = searcher.search(
            &text_query,
            &TopDocs::with_limit(num_candidates).order_by_score(),
        )?;
        let popular: Vec<DocAddress> = searcher
            .search(
                &text_query,
                &TopDocs::with_limit(POPULAR_CANDIDATES.min(num_docs))
                    .order_by_fast_field::<f64>(schema::LINK_SCORE, Order::Desc),
            )?
            .into_iter()
            .map(|(_, addr)| addr)
            .collect();
        // A site the query names is ranked even if BM25 put others first:
        // it is what the look-alikes are measured against. So is every site
        // of the kind the query names.
        let names = self.name_matches(&searcher, &query)?;
        let mut kinds = match &query.kind {
            Some(key) => matching_docs(
                &searcher,
                vec![Term::from_field_text(self.fields.kind_key, key)],
            )?,
            None => HashSet::new(),
        };
        // A kind of two words or more that a candidate's own title or
        // description names ("Wikipedia is a free online encyclopedia")
        // makes it one of that kind too: Wikidata does not tag every site
        // with every kind it is (wikipedia.org's item is a "Wikimedia
        // content project"). Single words ("airlines") are too often just
        // mentioned ("cheap airline tickets") to count.
        if !kinds.is_empty() && query.len >= 2 {
            let key = kind_key(query_text);
            for &(_, addr) in &candidates {
                if !kinds.contains(&addr)
                    && self.describes_itself_as(&searcher, addr, &key, query.len)?
                {
                    kinds.insert(addr);
                }
            }
        }
        // Meaning helps with queries that describe a site, not with names.
        let named_in_full = !kinds.is_empty() || names.values().any(|n| n.words() >= query.len);
        let meaning = meaning.filter(|_| !named_in_full);
        // The nearest sites in meaning, and the most popular of the next
        // nearest: among hundreds of thousands of sites, small ones whose
        // text repeats the query's words crowd out the big site it
        // describes, which may say little about itself.
        let nearest = match meaning {
            Some(meaning) => {
                let domains = meaning.nearest();
                let terms = |domains: &[String]| -> Vec<Term> {
                    domains
                        .iter()
                        .map(|domain| Term::from_field_text(self.fields.domain, domain))
                        .collect()
                };
                let split = domains.len().min(NEAREST_RANKED);
                let mut docs = matching_docs(&searcher, terms(&domains[..split]))?;
                let next = matching_docs(&searcher, terms(&domains[split..]))?;
                let mut next = link_scores(&searcher, &next);
                next.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                docs.extend(next.into_iter().take(NEAREST_POPULAR).map(|(_, addr)| addr));
                docs
            }
            None => HashSet::new(),
        };
        let known: HashSet<DocAddress> = candidates.iter().map(|&(_, addr)| addr).collect();
        let mut unranked: Vec<DocAddress> = names
            .keys()
            .chain(kinds.iter())
            .chain(nearest.iter())
            .chain(popular.iter())
            .filter(|addr| !known.contains(addr))
            .copied()
            .collect();
        unranked.sort_unstable();
        unranked.dedup();
        candidates.extend(bm25_of(&searcher, &text_query, unranked)?);
        let full_names: HashSet<DocAddress> = names
            .iter()
            .filter(|(_, name)| name.words() >= query.len)
            .map(|(&addr, _)| addr)
            .collect();
        let named = Named {
            words: names.values().map(NameMatch::words).max().unwrap_or(0),
            full_link_score: (!full_names.is_empty())
                .then(|| best_link_score(&searcher, &full_names)),
            kind: !kinds.is_empty(),
            typed: query.domain.is_some(),
        };
        if candidates.is_empty() {
            return Ok((SearchResults::default(), named));
        }

        let columns = searcher
            .segment_readers()
            .iter()
            .map(|segment| {
                let fast = segment.fast_fields();
                Ok(Columns {
                    link_scores: fast.f64(schema::LINK_SCORE)?,
                    domains: fast.str(schema::DOMAIN)?,
                    countries: fast.str(schema::COUNTRY)?,
                    languages: fast.str(schema::LANGUAGE)?,
                    adult: fast.u64(schema::ADULT)?,
                })
            })
            .collect::<tantivy::Result<Vec<_>>>()?;
        let link_score_of = |addr: DocAddress| {
            columns[addr.segment_ord as usize]
                .link_scores
                .first(addr.doc_id)
                .unwrap_or(0.0) as f32
        };

        let default = RankConfig::default();
        // A query is taken to describe what it looks for unless it names a
        // kind of thing, a site in full, or a well-known site by its first
        // words ("chase center tickets"; not code.gov in "code hosting").
        let navigational = named_in_full
            || query.domain.is_some()
            || names.iter().any(|(&addr, name)| {
                name.typed || (name.words() > 0 && link_score_of(addr) >= WELL_KNOWN_LINK_SCORE)
            });
        let alpha = match cfg.described_alpha {
            Some(described) if !navigational => unit_or(described, default.alpha),
            _ => unit_or(cfg.alpha, default.alpha),
        };
        let relevance_floor = cfg
            .described_relevance
            .filter(|floor| !navigational && *floor > 0.0);
        let partial_label_bonus = cfg.partial_label_bonus.unwrap_or(cfg.exact_label_bonus);
        let untrusted_share = unit_or(cfg.untrusted_share, default.untrusted_share);
        let country_boost = unit_or(cfg.country_boost, default.country_boost);
        let home = options.country.as_deref().and_then(normalize_country);
        // The evidence a site needs for its text to count in full: none
        // unless the query is a site's name plus more words, and never more
        // than that site has. A query that is just the name needs no guard:
        // the named site wins it on its own.
        let named_link_score = names
            .iter()
            .filter(|(_, name)| name.words() < query.len)
            .map(|(&addr, _)| link_score_of(addr))
            .fold(0.0, f32::max);
        let trusted_link_score =
            unit_or(cfg.trusted_link_score, default.trusted_link_score).min(named_link_score);
        // Sites the query names in full that nothing else says anything
        // about: never crawled (no title, description or Wikidata words),
        // no link text and no other name (from Wikidata or a redirect), and
        // less linked than the site the query names by its first words.
        // Such a bare name (you-tubemusic.com) is only a spelling of the
        // query, so it gets no more trust than a site with no links does:
        // youtube.com wins "youtube music". A site people link to by name
        // is not bare, crawled or not (homedepot.com, which turns crawlers
        // away, for "home depot"), nor is one with pages of its own
        // (json-schema.org for "json schema").
        let mut bare: HashSet<DocAddress> = HashSet::new();
        if trusted_link_score > 0.0 {
            for (&addr, name) in &names {
                if name.typed || name.words() < query.len || link_score_of(addr) >= named_link_score
                {
                    continue;
                }
                let segment = searcher.segment_reader(addr.segment_ord);
                let mut said = false;
                for field in [
                    self.fields.title,
                    self.fields.description,
                    self.fields.about,
                    self.fields.anchors,
                    self.fields.aliases,
                ] {
                    if let Some(norms) = segment.fieldnorms_readers().get_field(field)? {
                        said |= norms.fieldnorm(addr.doc_id) > 0;
                    }
                }
                if !said {
                    bare.insert(addr);
                }
            }
        }
        let query_words = query.len as f32;
        let meaning_weight = unit_or(cfg.meaning_weight, default.meaning_weight);

        // A site with no embedding (no text to make one from) is taken to
        // be as close in meaning as its words match, times the share of the
        // query they cover: electric.net matches "electric car maker" well
        // but by one word in three, so it is not put level with the sites
        // the query describes; schwab.com, matching all of "charles
        // schwab", is.
        let closeness_of = |addr: DocAddress| {
            meaning.and_then(|meaning| {
                columns[addr.segment_ord as usize]
                    .domain(addr.doc_id)
                    .and_then(|domain| meaning.closeness(&domain))
            })
        };
        let no_vector: Vec<DocAddress> = match meaning {
            Some(_) => candidates
                .iter()
                .map(|&(_, addr)| addr)
                .filter(|&addr| closeness_of(addr).is_none())
                .collect(),
            None => Vec::new(),
        };
        let coverage = query.coverage(&searcher, &self.fields, &no_vector)?;

        let max_bm25 = candidates.iter().map(|&(bm25, _)| bm25).fold(0.0, f32::max);
        let mut ranked: Vec<Ranked> = Vec::with_capacity(candidates.len());
        let language = options.language.as_deref().and_then(language_code);
        for (bm25, addr) in candidates {
            let column = &columns[addr.segment_ord as usize];
            if options.safe.hides(column.adult(addr.doc_id)) {
                continue;
            }
            let site_language = column.language(addr.doc_id);
            if let (Some(wanted), Some(site)) = (&language, &site_language) {
                if wanted != site {
                    continue;
                }
            }
            let country = column.country(addr.doc_id);
            let country_bonus = match (&home, &country) {
                (Some(home), Some(country)) if home == country => country_boost,
                (Some(_), Some(_)) if options.only_country => continue,
                (Some(_), Some(_)) => -country_boost,
                _ => 0.0,
            };
            let link_score = link_score_of(addr);
            let is_kind = kinds.contains(&addr);
            let name = names.get(&addr).copied().unwrap_or_default();
            let text_score = if is_kind || name.label >= query.len {
                // Being what the query names, or being named by all of it,
                // is a full match, however little of the site's own text
                // says so: among sites the query names in full, popularity
                // decides, so aa.com wins "american airlines" over
                // americanairlines.com.
                1.0
            } else {
                let words = if max_bm25 > 0.0 {
                    (bm25 / max_bm25).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let closeness =
                    closeness_of(addr).or_else(|| coverage.get(&addr).map(|&share| share * words));
                match closeness {
                    Some(closeness) => {
                        (1.0 - meaning_weight) * words + meaning_weight * closeness.clamp(0.0, 1.0)
                    }
                    None => words,
                }
            };
            let label_bonus = if name.label >= query.len {
                cfg.exact_label_bonus
            } else {
                partial_label_bonus
            };
            let mut name_bonus = (label_bonus * name.label as f32 / query_words)
                .max(cfg.exact_alias_bonus * name.alias as f32 / query_words);
            if is_kind {
                name_bonus = name_bonus.max(cfg.kind_bonus);
            }
            let trust = if name.typed || trusted_link_score <= 0.0 {
                1.0
            } else if bare.contains(&addr) {
                untrusted_share
            } else {
                let evidence = (link_score / trusted_link_score).min(1.0);
                untrusted_share + (1.0 - untrusted_share) * evidence
            };
            // Within a segment, term ordinals sort like the domains themselves.
            let domain_ord = column
                .domains
                .as_ref()
                .and_then(|domains| domains.term_ords(addr.doc_id).next())
                .unwrap_or(u64::MAX);
            let prior = match relevance_floor {
                Some(floor) if !is_kind && name.words() == 0 => {
                    link_score * (text_score / floor).min(1.0)
                }
                _ => link_score,
            };
            ranked.push(Ranked {
                addr,
                score: alpha * prior
                    + trust * ((1.0 - alpha) * text_score + name_bonus)
                    + country_bonus,
                text_score,
                link_score,
                country,
                named: name.typed || name.words() >= query.len,
                tie_break: (addr.segment_ord, domain_ord),
            });
        }
        ranked.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| b.link_score.total_cmp(&a.link_score))
                .then_with(|| a.tie_break.cmp(&b.tie_break))
        });

        // The best-ranked site named by the query's first words, with words
        // left over to search it for.
        let named_site = ranked.iter().find_map(|r| {
            let words = names.get(&r.addr)?.words();
            (words > 0 && words < query.len).then_some((r.addr, words))
        });
        let site_search = match named_site {
            Some((addr, words)) => self.site_search(&searcher, addr, query_text, words)?,
            None => None,
        };

        ranked.truncate(limit);
        let hits = ranked
            .into_iter()
            .map(|ranked| self.hit(&searcher, ranked))
            .collect::<Result<_>>()?;
        let results = SearchResults {
            hits,
            pages: Vec::new(),
            site_search,
            spelling: None,
        };
        Ok((results, named))
    }

    /// A link into the search of the site `domain` for `terms`, if the
    /// index has the site and it has a search address.
    fn site_search_of(&self, domain: &str, terms: &str) -> Result<Option<SiteSearch>> {
        let searcher = self.reader.searcher();
        let term = Term::from_field_text(self.fields.domain, domain);
        let Some(addr) = matching_docs(&searcher, vec![term])?.into_iter().next() else {
            return Ok(None);
        };
        self.site_search_link(&searcher, addr, terms.trim().to_string())
    }

    /// A link into the search of the site at `addr` for the words of `query`
    /// after its first `words`, if the site has a search address (its own,
    /// or one Plumb knows for big sites).
    fn site_search(
        &self,
        searcher: &tantivy::Searcher,
        addr: DocAddress,
        query: &str,
        words: usize,
    ) -> Result<Option<SiteSearch>> {
        let Some(terms) = words_after(&self.words, query, words) else {
            return Ok(None);
        };
        self.site_search_link(searcher, addr, terms)
    }

    /// A link into the search of the site at `addr` for `terms`.
    fn site_search_link(
        &self,
        searcher: &tantivy::Searcher,
        addr: DocAddress,
        terms: String,
    ) -> Result<Option<SiteSearch>> {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let text = |field| {
            doc.get_first(field)
                .and_then(|value| value.as_str())
                .map(str::to_string)
        };
        let Some(domain) = text(self.fields.domain) else {
            return Ok(None);
        };
        let link = text(self.fields.search_url)
            .and_then(|template| search_link(&template, &domain, &terms))
            .or_else(|| search_link(search_template_for(&domain)?, &domain, &terms));
        Ok(link.map(|url| SiteSearch { domain, terms, url }))
    }

    /// The sites whose domain label or an alias equals the query's first
    /// words, with how many words each covers, plus the site of a typed
    /// hostname (covering the whole query). An official site's Wikidata
    /// names count as labels (see [`schema`]).
    fn name_matches(
        &self,
        searcher: &tantivy::Searcher,
        query: &ParsedQuery,
    ) -> Result<HashMap<DocAddress, NameMatch>> {
        let mut names: HashMap<DocAddress, NameMatch> = HashMap::new();
        for (key, words) in &query.leading {
            let words = *words;
            let label = Term::from_field_text(self.fields.label_key, key);
            for addr in matching_docs(searcher, vec![label])? {
                let name = names.entry(addr).or_default();
                name.label = name.label.max(words);
            }
            let alias = Term::from_field_text(self.fields.alias_key, key);
            for addr in matching_docs(searcher, vec![alias])? {
                let name = names.entry(addr).or_default();
                name.alias = name.alias.max(words);
            }
        }
        if let Some(domain) = &query.domain {
            let domain = Term::from_field_text(self.fields.domain, domain);
            for addr in matching_docs(searcher, vec![domain])? {
                let name = names.entry(addr).or_default();
                name.label = query.len;
                name.typed = true;
            }
        }
        Ok(names)
    }

    /// Whether the document's title, description or Wikidata description
    /// holds `words` consecutive words whose [`kind_key`] is `key`.
    fn describes_itself_as(
        &self,
        searcher: &tantivy::Searcher,
        addr: DocAddress,
        key: &str,
        words: usize,
    ) -> Result<bool> {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let fields = [
            self.fields.title,
            self.fields.description,
            self.fields.about,
        ];
        Ok(fields.into_iter().any(|field| {
            doc.get_all(field)
                .filter_map(|value| value.as_str())
                .any(|text| names_kind(text, key, words))
        }))
    }

    /// Reads the stored fields of a ranked document.
    fn hit(&self, searcher: &tantivy::Searcher, ranked: Ranked) -> Result<Hit> {
        let doc: TantivyDocument = searcher.doc(ranked.addr)?;
        let text = |field| {
            doc.get_first(field)
                .and_then(|value| value.as_str())
                .map(str::to_string)
        };
        let domain = text(self.fields.domain).unwrap_or_default();
        let url = text(self.fields.url).unwrap_or_else(|| format!("https://{domain}/"));
        let about = text(self.fields.about);
        Ok(Hit {
            url,
            title: text(self.fields.title),
            official: about.is_some(),
            // The site's own description, else Wikipedia's, else what
            // Wikidata says it is.
            description: text(self.fields.description).or(about),
            domain,
            score: ranked.score,
            text_score: ranked.text_score,
            link_score: ranked.link_score,
            country: ranked.country,
            named: ranked.named,
            key_pages: text(self.fields.key_pages)
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default(),
        })
    }
}

/// How a query names sites, for typo correction.
#[derive(Debug, Clone, Copy, Default)]
struct Named {
    /// The most of the query's first words a site's name covers exactly.
    words: usize,
    /// The best link score among the sites the whole query names, if any.
    full_link_score: Option<f32>,
    /// The query names a kind of site that the index has.
    kind: bool,
    /// The query is a hostname or URL.
    typed: bool,
}

/// The text of `hit` that search operators look at, besides its domain.
fn hit_texts(hit: &Hit) -> impl Iterator<Item = &str> {
    [
        hit.title.as_deref(),
        hit.description.as_deref(),
        Some(hit.url.as_str()),
    ]
    .into_iter()
    .flatten()
}

/// The words of `query` before the [`INTENT_WORDS`] it ends with, if it
/// ends with any and has other words: "paypal login" -> "paypal".
pub fn without_intent_words(query: &str) -> Option<String> {
    let mut words: Vec<String> = normalize_text(query)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let all = words.len();
    loop {
        // The longest that fits: "web docs" rather than "docs".
        let cut = INTENT_WORDS
            .iter()
            .filter_map(|intent| {
                let n = intent.split(' ').count();
                let tail = words.get(words.len().checked_sub(n)?..)?;
                (n < words.len() && tail.iter().map(String::as_str).eq(intent.split(' ')))
                    .then_some(n)
            })
            .max();
        match cut {
            Some(n) => words.truncate(words.len() - n),
            None => break,
        }
    }
    (words.len() < all).then(|| words.join(" "))
}

/// The best link score among `docs`, 0 for none.
fn best_link_score(searcher: &tantivy::Searcher, docs: &HashSet<DocAddress>) -> f32 {
    link_scores(searcher, docs)
        .into_iter()
        .map(|(score, _)| score)
        .fold(0.0, f32::max)
}

/// The link score of each of `docs` that has one.
fn link_scores(searcher: &tantivy::Searcher, docs: &HashSet<DocAddress>) -> Vec<(f32, DocAddress)> {
    let mut columns: HashMap<u32, Option<tantivy::columnar::Column<f64>>> = HashMap::new();
    let mut scores = Vec::with_capacity(docs.len());
    for &addr in docs {
        let column = columns.entry(addr.segment_ord).or_insert_with(|| {
            searcher
                .segment_reader(addr.segment_ord)
                .fast_fields()
                .f64(schema::LINK_SCORE)
                .ok()
        });
        if let Some(score) = column.as_ref().and_then(|c| c.first(addr.doc_id)) {
            scores.push((score as f32, addr));
        }
    }
    scores
}

/// A candidate with its blended score.
struct Ranked {
    addr: DocAddress,
    score: f32,
    text_score: f32,
    link_score: f32,
    country: Option<String>,
    named: bool,
    tie_break: (u32, u64),
}

/// The fast columns of one segment.
struct Columns {
    link_scores: tantivy::columnar::Column<f64>,
    domains: Option<tantivy::columnar::StrColumn>,
    countries: Option<tantivy::columnar::StrColumn>,
    languages: Option<tantivy::columnar::StrColumn>,
    adult: tantivy::columnar::Column<u64>,
}

impl Columns {
    fn domain(&self, doc: tantivy::DocId) -> Option<String> {
        let domains = self.domains.as_ref()?;
        let ord = domains.term_ords(doc).next()?;
        let mut domain = String::new();
        domains.ord_to_str(ord, &mut domain).ok()?;
        Some(domain)
    }

    fn country(&self, doc: tantivy::DocId) -> Option<String> {
        let countries = self.countries.as_ref()?;
        let ord = countries.term_ords(doc).next()?;
        let mut country = String::new();
        countries.ord_to_str(ord, &mut country).ok()?;
        (!country.is_empty()).then_some(country)
    }

    fn language(&self, doc: tantivy::DocId) -> Option<String> {
        let languages = self.languages.as_ref()?;
        let ord = languages.term_ords(doc).next()?;
        let mut language = String::new();
        languages.ord_to_str(ord, &mut language).ok()?;
        (!language.is_empty()).then_some(language)
    }

    fn adult(&self, doc: tantivy::DocId) -> AdultLevel {
        schema::adult_from(self.adult.first(doc).unwrap_or(0))
    }
}

/// The words of `query` after the first `words` of its analyzed words, as
/// typed: "github sueheir/plumb-search" after 1 -> "sueheir/plumb-search".
/// `None` when nothing is left, or when the typed words do not line up
/// with the analyzed ones.
fn words_after(analyzer: &TextAnalyzer, query: &str, words: usize) -> Option<String> {
    let typed: Vec<&str> = query.split_whitespace().collect();
    let mut seen = 0;
    for (i, word) in typed.iter().enumerate() {
        if seen == words {
            let rest = typed[i..].join(" ");
            return (!rest.is_empty()).then_some(rest);
        }
        seen += analysis::tokens(analyzer, word).len();
        if seen > words {
            return None;
        }
    }
    None
}

/// How many of the query's words, from the first on, a site's names cover.
#[derive(Debug, Clone, Copy, Default)]
struct NameMatch {
    /// Words covered by the domain label: 2 for usbank.com in "us bank login".
    label: usize,
    /// Words covered by an alias.
    alias: usize,
    /// The query is this site's hostname or URL.
    typed: bool,
}

impl NameMatch {
    /// Words covered by the site's best name.
    fn words(&self) -> usize {
        self.label.max(self.alias)
    }
}

/// `value` clamped to `0..=1`, or `default` when it is not a number.
fn unit_or(value: f32, default: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        default
    }
}

/// The BM25 score of each of `docs` for `query`, 0 where it does not match.
fn bm25_of(
    searcher: &tantivy::Searcher,
    query: &dyn Query,
    mut docs: Vec<DocAddress>,
) -> Result<Vec<(f32, DocAddress)>> {
    if docs.is_empty() {
        return Ok(Vec::new());
    }
    docs.sort_unstable();
    let weight = query.weight(EnableScoring::enabled_from_searcher(searcher))?;
    let mut scored = Vec::with_capacity(docs.len());
    let mut scorer: Option<(u32, Box<dyn Scorer>)> = None;
    for addr in docs {
        let scorer = match &mut scorer {
            Some((segment_ord, scorer)) if *segment_ord == addr.segment_ord => scorer,
            _ => {
                let reader = searcher.segment_reader(addr.segment_ord);
                let segment_scorer = weight.scorer(reader, 1.0)?;
                &mut scorer.insert((addr.segment_ord, segment_scorer)).1
            }
        };
        // Documents come in order, so the scorer only moves forward.
        if scorer.doc() < addr.doc_id {
            scorer.seek(addr.doc_id);
        }
        let bm25 = if scorer.doc() == addr.doc_id {
            scorer.score()
        } else {
            0.0
        };
        scored.push((bm25, addr));
    }
    Ok(scored)
}

/// The documents containing any of `terms`.
fn matching_docs(searcher: &tantivy::Searcher, terms: Vec<Term>) -> Result<HashSet<DocAddress>> {
    if terms.is_empty() {
        return Ok(HashSet::new());
    }
    let clauses = terms
        .into_iter()
        .map(|term| {
            let query: Box<dyn Query> = Box::new(TermQuery::new(term, IndexRecordOption::Basic));
            (Occur::Should, query)
        })
        .collect();
    Ok(searcher.search(&BooleanQuery::new(clauses), &DocSetCollector)?)
}

/// A user query, analyzed the same way as the indexed text.
#[derive(Debug, PartialEq)]
struct ParsedQuery {
    /// Distinct words, in query order: `U.S. Bank` -> `us`, `bank`.
    words: Vec<String>,
    /// Each word in its other number ([`plumb_core::other_number`]), if it
    /// has one: `videos` -> `video`.
    others: Vec<Option<String>>,
    /// The whole query as one joined token: `U.S. Bank` -> `usbank`.
    joined: Option<String>,
    /// The first word, the first two joined, and so on (at most
    /// [`MAX_QUERY_WORDS`]), each with the number of query words it covers:
    /// `us bank login` -> `us` (1), `usbank` (2), `usbanklogin` (3). The
    /// names a site can have to be named by the query. A leading "the" may
    /// be left out: `the new york times` also gives `newyorktimes` (4).
    leading: Vec<(String, usize)>,
    /// The whole query as a kind ([`plumb_core::kind_key`]): `banks` -> `bank`.
    kind: Option<String>,
    /// Number of words in the query, repeats included.
    len: usize,
    /// The registrable domain, when the query is a hostname or URL.
    domain: Option<String>,
}

impl ParsedQuery {
    /// `None` when the query has no letters or digits.
    fn new(query: &str, words: &TextAnalyzer, joined: &TextAnalyzer) -> Option<ParsedQuery> {
        let query = truncate_chars(query, MAX_TEXT_CHARS);
        let tokens = analysis::tokens(words, &query);
        if tokens.is_empty() {
            return None;
        }
        let mut distinct = Vec::new();
        for word in &tokens {
            if !distinct.contains(word) {
                distinct.push(word.clone());
            }
        }
        distinct.truncate(MAX_QUERY_WORDS);
        let others = distinct.iter().map(|word| other_number(word)).collect();
        let prefixes = |skip: usize| {
            tokens
                .iter()
                .skip(skip)
                .take(MAX_QUERY_WORDS)
                .scan(String::new(), move |key, word| {
                    key.push_str(word);
                    Some(key.clone())
                })
                .enumerate()
                .map(move |(i, key)| (key, skip + i + 1))
        };
        let mut leading: Vec<(String, usize)> = prefixes(0)
            .filter(|(key, words)| tokens.len() == 1 || *words > 1 || !is_function_word(key))
            .collect();
        if tokens.len() > 1 && tokens[0] == "the" {
            leading.extend(prefixes(1));
        }
        let kind = analysis::tokens(joined, &kind_key(&query))
            .into_iter()
            .next();
        Some(ParsedQuery {
            words: distinct,
            others,
            joined: analysis::tokens(joined, &query).into_iter().next(),
            leading,
            kind,
            len: tokens.len(),
            domain: typed_domain(&query),
        })
    }

    /// One boosted BM25 clause per word and field, plus the joined query on
    /// the joined and label fields and, for a typed domain, the domain itself.
    ///
    /// The label and joined fields hold whole names, so in a query of `n`
    /// words a single word matching there gets `1/n` of the field's boost:
    /// for "us bank online banking", bank.com (whose whole name is one of
    /// the words) must not outweigh usbank.com matching every word elsewhere.
    /// The fields each query word is searched in, with their boosts.
    fn per_word(&self, f: &Fields) -> [(Field, f32); 8] {
        let name_share = 1.0 / self.words.len() as f32;
        [
            (f.label, LABEL_BOOST * name_share),
            (f.joined, JOINED_BOOST * name_share),
            (f.aliases, ALIASES_BOOST),
            (f.title, TITLE_BOOST),
            (f.anchors, ANCHORS_BOOST),
            (f.description, DESCRIPTION_BOOST),
            (f.headings, HEADINGS_BOOST),
            (f.about, ABOUT_BOOST),
        ]
    }

    /// The clauses for the `i`th query word: the word in every field, and
    /// its other number in the fields of free text, a little weaker, so
    /// "videos" finds a site about "video" but names stay exact.
    ///
    /// The other number never counts for more than the word as typed: when
    /// "videos" is common and "video" rare, BM25 would make the rare form
    /// outweigh every site that has the query's own words, so its boost is
    /// scaled down to the typed word's rarity.
    fn word_clauses(
        &self,
        i: usize,
        searcher: &tantivy::Searcher,
        f: &Fields,
        clauses: &mut Clauses,
    ) -> Result<()> {
        let word = &self.words[i];
        // Only a word joining two others: "to" in "to do list" is a word
        // of the thing looked for.
        let joining = i > 0 && i + 1 < self.words.len() && is_function_word(word);
        let names = self.len == 1 || !joining;
        for (field, boost) in self.per_word(f) {
            if !names && (field == f.label || field == f.joined) {
                continue;
            }
            clauses.add(Term::from_field_text(field, word), boost);
        }
        let Some(other) = &self.others[i] else {
            return Ok(());
        };
        let docs = searcher.num_docs() as f32;
        let rarity = |term: &Term| -> Result<f32> {
            let found = searcher.doc_freq(term)? as f32;
            Ok((1.0 + (docs - found + 0.5) / (found + 0.5)).ln())
        };
        for (field, boost) in self.per_word(f) {
            if field == f.label || field == f.joined {
                continue;
            }
            let other = Term::from_field_text(field, other);
            let other_rarity = rarity(&other)?;
            if other_rarity <= 0.0 {
                continue;
            }
            let share = (rarity(&Term::from_field_text(field, word))? / other_rarity).min(1.0);
            clauses.add(other, boost * OTHER_NUMBER_SHARE * share);
        }
        Ok(())
    }

    /// The share of the query's words each of `docs` has in its text, in
    /// the order given: 1 for a site whose name is the whole query joined.
    fn coverage(
        &self,
        searcher: &tantivy::Searcher,
        f: &Fields,
        docs: &[DocAddress],
    ) -> Result<HashMap<DocAddress, f32>> {
        let mut covered: HashMap<DocAddress, f32> = docs.iter().map(|&addr| (addr, 0.0)).collect();
        if docs.is_empty() {
            return Ok(covered);
        }
        let share = 1.0 / self.words.len() as f32;
        for i in 0..self.words.len() {
            let mut clauses = Clauses::default();
            self.word_clauses(i, searcher, f, &mut clauses)?;
            for (bm25, addr) in bm25_of(searcher, &clauses.into_query(), docs.to_vec())? {
                if bm25 > 0.0 {
                    *covered.entry(addr).or_default() += share;
                }
            }
        }
        if let Some(joined) = &self.joined {
            let mut clauses = Clauses::default();
            clauses.add(Term::from_field_text(f.joined, joined), 1.0);
            clauses.add(Term::from_field_text(f.label, joined), 1.0);
            for (bm25, addr) in bm25_of(searcher, &clauses.into_query(), docs.to_vec())? {
                if bm25 > 0.0 {
                    covered.insert(addr, 1.0);
                }
            }
        }
        for share in covered.values_mut() {
            *share = share.min(1.0);
        }
        Ok(covered)
    }

    fn text_query(&self, searcher: &tantivy::Searcher, f: &Fields) -> Result<BooleanQuery> {
        let mut clauses = Clauses::default();
        for i in 0..self.words.len() {
            self.word_clauses(i, searcher, f, &mut clauses)?;
        }
        if let Some(joined) = &self.joined {
            clauses.add(Term::from_field_text(f.joined, joined), WHOLE_QUERY_BOOST);
            clauses.add(Term::from_field_text(f.label, joined), WHOLE_QUERY_BOOST);
        }
        if let Some(domain) = &self.domain {
            clauses.add(Term::from_field_text(f.domain, domain), DOMAIN_BOOST);
        }
        Ok(clauses.into_query())
    }
}

/// Boosted term clauses of a disjunction. A term added twice keeps the
/// larger boost (in a one-word query, the word is also the whole query).
#[derive(Default)]
struct Clauses(Vec<(Term, f32)>);

impl Clauses {
    fn add(&mut self, term: Term, boost: f32) {
        match self.0.iter_mut().find(|(known, _)| *known == term) {
            Some((_, known_boost)) => *known_boost = known_boost.max(boost),
            None => self.0.push((term, boost)),
        }
    }

    fn into_query(self) -> BooleanQuery {
        let clauses = self
            .0
            .into_iter()
            .map(|(term, boost)| {
                // Fields indexed without frequencies fall back to `Basic`.
                let term_query = TermQuery::new(term, IndexRecordOption::WithFreqs);
                let query: Box<dyn Query> = Box::new(BoostQuery::new(Box::new(term_query), boost));
                (Occur::Should, query)
            })
            .collect();
        BooleanQuery::new(clauses)
    }
}

/// The registrable domain of a query that looks like a hostname or URL.
fn typed_domain(query: &str) -> Option<String> {
    let query = query.trim();
    if !query.contains('.') || query.contains(char::is_whitespace) {
        return None;
    }
    registrable_domain(query)
}

#[allow(dead_code)]
fn assert_searcher_is_send_sync() {
    fn check<T: Send + Sync>() {}
    check::<Searcher>();
}

/// Whether `text` holds `words` consecutive words whose [`kind_key`] is
/// `key` at the end of a phrase: "a free online encyclopedia, made by"
/// names `onlineencyclopedia`, but "search engine optimization tools" does
/// not name `searchengine`. A phrase ends at punctuation, the end of the
/// text, or a word such as "that", "for" or "in".
fn names_kind(text: &str, key: &str, words: usize) -> bool {
    const ENDS: &[&str] = &[
        "that", "which", "who", "where", "for", "with", "by", "from", "in", "of", "on", "and",
        "or", "to", "since", "founded", "based", "run", "made", "created",
    ];
    if words == 0 {
        return false;
    }
    let breaks = |c: char| {
        matches!(
            c,
            ',' | '.' | ';' | ':' | '!' | '?' | '(' | ')' | '|' | '\u{2013}' | '\u{2014}'
        )
    };
    text.split(breaks).any(|clause| {
        let normalized = plumb_core::normalize_text(clause);
        let tokens: Vec<&str> = normalized.split(' ').filter(|w| !w.is_empty()).collect();
        tokens.windows(words).enumerate().any(|(i, window)| {
            let next = tokens.get(i + words);
            next.is_none_or(|next| ENDS.contains(next)) && kind_key(&window.join(" ")) == key
        })
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::time::Instant;

    use plumb_core::{LinkText, Signals};
    use tempfile::TempDir;

    use super::*;

    /// A record with the given page fields, aliases and link texts.
    fn site(
        domain: &str,
        title: Option<&str>,
        description: Option<&str>,
        aliases: &[&str],
        link_texts: &[(&str, u32)],
        signals: Signals,
    ) -> SiteRecord {
        let mut record = SiteRecord::new(domain);
        record.title = title.map(str::to_string);
        record.description = description.map(str::to_string);
        for alias in aliases {
            record.add_alias(alias);
        }
        for &(text, count) in link_texts {
            let text = plumb_core::normalize_text(text);
            if !text.is_empty() {
                record.link_texts.push(LinkText::with_count(text, count));
            }
        }
        record.signals = signals;
        record
    }

    fn popular(tranco_rank: u32, linking_domains: u32) -> Signals {
        Signals {
            tranco_rank: Some(tranco_rank),
            linking_domains,
            official_site: true,
            ..Default::default()
        }
    }

    fn obscure(harmonic_rank: u64, linking_domains: u32) -> Signals {
        Signals {
            harmonic_rank: Some(harmonic_rank),
            linking_domains,
            ..Default::default()
        }
    }

    /// Real brands plus decoys that stuff the brand name into their own pages.
    fn corpus() -> Vec<SiteRecord> {
        let mut usbank = site(
            "usbank.com",
            Some("U.S. Bank | Personal Banking, Credit Cards, Home Loans & More"),
            Some(
                "Discover U.S. Bank: checking and savings accounts, credit cards, \
                 mortgages, auto loans and investing.",
            ),
            &["U.S. Bank", "U.S. Bancorp"],
            &[
                ("US Bank", 1200),
                ("U.S. Bank", 300),
                ("usbank", 300),
                ("US Bank login", 40),
                ("online banking", 25),
            ],
            popular(950, 42_000),
        );
        usbank.url = Some("https://www.usbank.com/".into());

        let mut boa = site(
            "bankofamerica.com",
            Some("Bank of America - Banking, Credit Cards, Loans and Merrill Investing"),
            Some(
                "What would you like the power to do? At Bank of America, our purpose \
                 is to help make financial lives better.",
            ),
            &["Bank of America"],
            &[
                ("Bank of America", 2500),
                ("BofA", 300),
                ("bankofamerica.com", 120),
                ("online banking", 80),
            ],
            popular(310, 88_000),
        );
        boa.url = Some("https://www.bankofamerica.com/".into());

        let mut allybank = site("allybank.com", None, None, &[], &[], obscure(2_000_000, 4));
        allybank.url = Some("https://www.ally.com/".into());

        vec![
            usbank,
            site(
                "usbank-login-help.com",
                Some("US Bank Login Help"),
                Some("Step by step help with your US Bank login and US Bank online banking."),
                &[],
                &[],
                Signals::default(),
            ),
            site(
                "usbankreviews.net",
                Some("US Bank Reviews - Is US Bank Good?"),
                Some("Customer reviews of US Bank checking, savings and US Bank credit cards."),
                &["US Bank Reviews"],
                &[("us bank reviews", 3)],
                obscure(9_000_000, 3),
            ),
            boa,
            site(
                "americabank.com",
                Some("America Bank | Community Banking in Texas"),
                Some("America Bank offers personal and business banking."),
                &[],
                &[("America Bank", 6)],
                obscure(4_500_000, 12),
            ),
            site(
                "bankofamerica-fraud-alerts.com",
                Some("Bank of America Fraud Alerts"),
                Some("Report Bank of America phishing and fraud. Bank of America alerts."),
                &[],
                &[],
                Signals::default(),
            ),
            site(
                "chase.com",
                Some("Chase Bank - Credit Cards, Mortgages, Commercial Banking, Auto Loans"),
                None,
                &["Chase"],
                &[("Chase", 3000), ("Chase Bank", 900)],
                popular(120, 120_000),
            ),
            site(
                "ally.com",
                Some("Ally Bank | Online Banking, Savings, Auto Loans & Investing"),
                None,
                &["Ally Bank", "Ally Financial"],
                &[("Ally Bank", 600), ("Ally", 400)],
                popular(2_500, 30_000),
            ),
            allybank,
            site(
                "nestle.com",
                Some("Nestlé Global"),
                Some("Good food, good life."),
                &["Nestlé"],
                &[("Nestlé", 400), ("nestle", 150)],
                popular(4_000, 15_000),
            ),
            site(
                "acme.com",
                Some("Acme Corporation"),
                None,
                &[],
                &[],
                obscure(20_000, 2_000),
            ),
            site(
                "acme.net",
                Some("Acme Corporation"),
                None,
                &[],
                &[],
                Signals::default(),
            ),
            site(
                "wikipedia.org",
                Some("Wikipedia, the free encyclopedia"),
                None,
                &["Wikipedia"],
                &[("Wikipedia", 9000)],
                popular(10, 1_000_000),
            ),
            site(
                "xn--bcher-kva.de",
                Some("Bücher online kaufen"),
                None,
                &[],
                &[],
                obscure(300_000, 40),
            ),
        ]
    }

    fn build(records: &[SiteRecord]) -> (TempDir, Searcher) {
        let dir = TempDir::new().unwrap();
        let stats = build_index(dir.path(), records).unwrap();
        assert_eq!(stats.docs, records.len() as u64);
        let searcher = Searcher::open(dir.path()).unwrap();
        (dir, searcher)
    }

    fn domains(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.domain.as_str()).collect()
    }

    fn top(searcher: &Searcher, query: &str) -> String {
        let hits = searcher.search(query, 10).unwrap();
        assert!(!hits.is_empty(), "no hits for {query:?}");
        hits[0].domain.clone()
    }

    fn ranked(tranco_rank: u32, linking_domains: u32) -> Signals {
        Signals {
            official_site: false,
            ..popular(tranco_rank, linking_domains)
        }
    }

    fn with_facts(mut record: SiteRecord, country: Option<&str>, kinds: &[&str]) -> SiteRecord {
        record.country = country.map(str::to_string);
        for kind in kinds {
            record.add_kind(kind);
        }
        record
    }

    #[test]
    fn the_networks_own_site_comes_first_for_its_name() {
        let records = vec![
            plumb_core::home_site_record(),
            site(
                "plumbs.com",
                Some("Plumbs"),
                None,
                &[],
                &[],
                popular(5_000, 2_000),
            ),
            site(
                "plumbfounded.com",
                Some("Plumbfounded Plumbing"),
                None,
                &[],
                &[],
                ranked(40_000, 200),
            ),
            site(
                "search.com",
                Some("Search"),
                None,
                &[],
                &[],
                popular(3_000, 5_000),
            ),
        ];
        let (_dir, searcher) = build(&records);
        for query in ["plumbsearch", "plumbsearch.org"] {
            let hits = searcher.search(query, 10).unwrap();
            assert_eq!(hits[0].domain, plumb_core::HOME_SITE, "{query}: {hits:?}");
        }
    }

    #[test]
    fn wikidata_descriptions_are_searched_and_shown() {
        let mut navy = site("navyfederal.org", None, None, &[], &[], ranked(4_000, 500));
        navy.about = Some("American credit union".into());
        let records = vec![
            navy,
            site("navy.mil", None, None, &[], &[], popular(2_000, 9_000)),
            site("credit.com", None, None, &[], &[], ranked(30_000, 300)),
        ];
        let (_dir, searcher) = build(&records);
        // Names still count for more than descriptions, but a site that
        // only its description matches is found, and shows it.
        let hits = searcher.search("american credit union", 10).unwrap();
        let navy = hits.iter().find(|hit| hit.domain == "navyfederal.org");
        assert_eq!(
            navy.and_then(|hit| hit.description.as_deref()),
            Some("American credit union"),
            "{hits:?}"
        );
    }

    #[test]
    fn sites_that_redirect_name_the_site_they_redirect_to() {
        let mut lookalike = site(
            "pncbank.com",
            Some("PNC Bank"),
            None,
            &[],
            &[],
            ranked(9_000, 400),
        );
        lookalike.redirect = Some(plumb_core::Redirect {
            to: "pnc.com".into(),
            at: 1,
        });
        // A redirect to a site not in the index changes nothing.
        let mut elsewhere = site("fb.example", None, None, &[], &[], ranked(50_000, 10));
        elsewhere.redirect = Some(plumb_core::Redirect {
            to: "facebook.example".into(),
            at: 1,
        });
        let records = vec![
            site("pnc.com", None, None, &[], &[], ranked(2_000, 900)),
            lookalike,
            elsewhere,
            site("bank.com", None, None, &[], &[], ranked(3_000, 900)),
        ];
        let dir = TempDir::new().unwrap();
        let stats = build_index(dir.path(), &records).unwrap();
        assert_eq!((stats.docs, stats.redirected), (3, 1));
        let searcher = Searcher::open(dir.path()).unwrap();
        let hits = searcher.search("pnc bank", 10).unwrap();
        assert_eq!(hits[0].domain, "pnc.com", "{hits:?}");
        assert!(domains(&hits).iter().all(|d| *d != "pncbank.com"));
        assert_eq!(top(&searcher, "pncbank"), "pnc.com");
        assert_eq!(top(&searcher, "fb example"), "fb.example");
    }

    /// A [`Meaning`] with fixed closeness per domain.
    struct FixedMeaning(Vec<(&'static str, f32)>);

    impl Meaning for FixedMeaning {
        fn nearest(&self) -> Vec<String> {
            self.0
                .iter()
                .map(|(domain, _)| domain.to_string())
                .collect()
        }

        fn closeness(&self, domain: &str) -> Option<f32> {
            self.0
                .iter()
                .find(|(d, _)| *d == domain)
                .map(|&(_, closeness)| closeness)
        }
    }

    #[test]
    fn meaning_finds_described_sites_but_leaves_names_alone() {
        let records = vec![
            site(
                "tesla.com",
                None,
                None,
                &["Tesla"],
                &[],
                popular(500, 20_000),
            ),
            site(
                "rivian.com",
                None,
                None,
                &["Rivian"],
                &[],
                popular(9_000, 3_000),
            ),
            site("electric.com", None, None, &[], &[], ranked(40_000, 200)),
            site("carmaker.net", None, None, &[], &[], ranked(90_000, 50)),
        ];
        let (_dir, searcher) = build(&records);
        let cfg = RankConfig::default();
        let options = SearchOptions::default();
        let meaning = FixedMeaning(vec![
            ("tesla.com", 0.9),
            ("rivian.com", 0.85),
            ("electric.com", 0.2),
        ]);
        let search = |query: &str, meaning: Option<&dyn Meaning>| {
            let results = searcher
                .search_meaning(query, 10, &cfg, &options, meaning)
                .unwrap();
            results
                .hits
                .into_iter()
                .map(|hit| hit.domain)
                .collect::<Vec<_>>()
        };
        // Without meaning, the words win; tesla.com and rivian.com share
        // none with the query.
        let by_words = search("electric car maker", None);
        assert_eq!(by_words.first().map(String::as_str), Some("electric.com"));
        assert!(!by_words.contains(&"tesla.com".to_string()), "{by_words:?}");
        // With it, the sites the query describes come first.
        let by_meaning = search("electric car maker", Some(&meaning));
        assert_eq!(
            &by_meaning[..2],
            ["tesla.com", "rivian.com"],
            "{by_meaning:?}"
        );
        // A site with no embedding that one query word matches does not
        // beat the sites the query describes (as on plumbsearch.org, where
        // electric.net and friends had no vector)...
        let unembedded = FixedMeaning(vec![("tesla.com", 1.0), ("rivian.com", 0.9)]);
        assert_eq!(
            &search("electric car maker", Some(&unembedded))[..2],
            ["tesla.com", "rivian.com"]
        );
        // ...but keeps its word match over sites far in meaning.
        let partial = FixedMeaning(vec![("tesla.com", 0.3)]);
        assert_eq!(
            search("electric car maker", Some(&partial))[0],
            "electric.com"
        );
        // A query naming a site is ranked as before.
        let named = FixedMeaning(vec![("tesla.com", 1.0)]);
        assert_eq!(search("rivian", Some(&named))[0], "rivian.com");
        assert_eq!(search("rivian", Some(&named)).len(), 1);
    }

    #[test]
    fn sites_with_no_embedding_matching_the_whole_query_keep_their_words() {
        let records = vec![
            site(
                "schwab.com",
                Some("Charles Schwab"),
                None,
                &[],
                &[],
                ranked(1_500, 3_000),
            ),
            site(
                "wsj.com",
                None,
                None,
                &["The Wall Street Journal"],
                &[],
                popular(300, 40_000),
            ),
        ];
        let (_dir, searcher) = build(&records);
        // The query names no site in full, so meaning counts, and wsj.com
        // is the nearest site with a vector; schwab.com has none.
        let meaning = FixedMeaning(vec![("wsj.com", 1.0)]);
        let hits = searcher
            .search_meaning(
                "charles schwab",
                10,
                &RankConfig::default(),
                &SearchOptions::default(),
                Some(&meaning),
            )
            .unwrap()
            .hits;
        assert_eq!(hits[0].domain, "schwab.com", "{hits:?}");
    }

    /// Short official domains and the spelled-out or one-word domains that
    /// beat them before Wikidata names counted as labels.
    fn short_names_corpus() -> Vec<SiteRecord> {
        vec![
            site(
                "wsj.com",
                None,
                None,
                &["The Wall Street Journal"],
                &[],
                popular(300, 40_000),
            ),
            site("wall.org", None, None, &[], &[], ranked(20_000, 900)),
            site("wallstreet.com", None, None, &[], &[], ranked(90_000, 200)),
            site(
                "nytimes.com",
                None,
                None,
                &["The New York Times"],
                &[],
                popular(80, 90_000),
            ),
            // Look-alikes spell the whole name out in their domain and
            // title, so they match the words better than the real site.
            site(
                "newyorktimes.com",
                Some("New York Times | New York Times News"),
                Some("New York Times news from New York."),
                &[],
                &[],
                ranked(197_000, 40),
            ),
            site(
                "aa.com",
                None,
                None,
                &["American Airlines"],
                &[],
                popular(900, 12_000),
            ),
            site(
                "americanairlines.com",
                Some("American Airlines | American Airlines flights"),
                None,
                &[],
                &[],
                ranked(506_000, 20),
            ),
            site(
                "americanairlines.fr",
                None,
                None,
                &[],
                &[],
                ranked(60_000, 300),
            ),
        ]
    }

    #[test]
    fn short_official_names_beat_spelled_out_domains() {
        let (_dir, searcher) = build(&short_names_corpus());
        for (query, expected) in [
            ("wall street journal", "wsj.com"),
            ("the wall street journal", "wsj.com"),
            ("wsj", "wsj.com"),
            ("new york times", "nytimes.com"),
            ("The New York Times", "nytimes.com"),
            ("american airlines", "aa.com"),
            ("wall", "wall.org"),
        ] {
            assert_eq!(top(&searcher, query), expected, "{query}");
        }
    }

    #[test]
    fn unofficial_aliases_stay_weaker_than_labels() {
        // Anyone can call their site anything in og:site_name.
        let records = vec![
            site(
                "usbank.com",
                None,
                None,
                &["U.S. Bank"],
                &[],
                popular(1_500, 9_000),
            ),
            site(
                "cheap-loans.biz",
                None,
                None,
                &["US Bank"],
                &[],
                ranked(800_000, 3),
            ),
        ];
        let (_dir, searcher) = build(&records);
        assert_eq!(top(&searcher, "us bank"), "usbank.com");
    }

    fn bank_corpus() -> Vec<SiteRecord> {
        let bank = |domain: &str, alias: &str, country: &str, rank: u32| {
            with_facts(
                site(domain, None, None, &[alias], &[], popular(rank, 5_000)),
                Some(country),
                &["bank", "public company"],
            )
        };
        vec![
            bank("chase.com", "Chase Bank", "US", 150),
            bank("wellsfargo.com", "Wells Fargo", "US", 250),
            bank("usbank.com", "U.S. Bank", "US", 1_500),
            bank("db.com", "Deutsche Bank", "DE", 2_000),
            bank("commerzbank.de", "Commerzbank", "DE", 3_000),
            bank("sparkasse.de", "Sparkasse", "DE", 1_800),
            with_facts(
                site(
                    "airbus.com",
                    None,
                    None,
                    &["Airbus"],
                    &[],
                    popular(2_500, 4_000),
                ),
                None,
                &["aircraft manufacturer"],
            ),
            site(
                "bankrate.com",
                Some("Bankrate: banks, mortgage rates and savings accounts"),
                None,
                &[],
                &[("banks", 50)],
                ranked(2_000, 6_000),
            ),
        ]
    }

    fn options(country: &str, only_country: bool) -> SearchOptions {
        SearchOptions {
            country: Some(country.to_string()),
            only_country,
            exact: false,
            ..SearchOptions::default()
        }
    }

    fn search_in(searcher: &Searcher, query: &str, options: &SearchOptions) -> Vec<String> {
        searcher
            .search_full(query, 10, &RankConfig::default(), options)
            .unwrap()
            .hits
            .into_iter()
            .map(|hit| hit.domain)
            .collect()
    }

    #[test]
    fn kinds_list_the_banks_of_your_country_first() {
        let (_dir, searcher) = build(&bank_corpus());
        let us = search_in(&searcher, "banks", &options("US", false));
        assert_eq!(
            us[..3],
            ["chase.com", "wellsfargo.com", "usbank.com"],
            "{us:?}"
        );
        let de = search_in(&searcher, "banks", &options("de", false));
        assert_eq!(
            de[..3],
            ["sparkasse.de", "db.com", "commerzbank.de"],
            "{de:?}"
        );
        // Singular works too, and every bank is listed even without text matches.
        let bank = search_in(&searcher, "bank", &options("US", false));
        for domain in ["chase.com", "db.com", "commerzbank.de", "sparkasse.de"] {
            assert!(bank.contains(&domain.to_string()), "{bank:?}");
        }
        assert!(!bank.contains(&"airbus.com".to_string()));
        // Generic kinds are not kept, so they find nothing by kind.
        assert!(search_in(&searcher, "public companies", &options("US", false)).is_empty());
    }

    #[test]
    fn a_site_that_says_it_is_of_a_kind_joins_the_kind() {
        let wikipedia = with_facts(
            site(
                "wikipedia.org",
                Some("Wikipedia"),
                Some("Wikipedia is a free online encyclopedia, created and edited by volunteers"),
                &[],
                &[],
                ranked(30, 50_000),
            ),
            None,
            &["Wikimedia content project"],
        );
        let grok = with_facts(
            site(
                "grokipedia.com",
                Some("Grokipedia"),
                None,
                &[],
                &[],
                ranked(40_000, 300),
            ),
            None,
            &["online encyclopedia"],
        );
        // A popular site that mentions a single-word kind is not one.
        let kayak = site(
            "kayak.com",
            Some("KAYAK"),
            Some("Cheap flights and airline tickets"),
            &[],
            &[],
            ranked(200, 20_000),
        );
        let delta = with_facts(
            site(
                "delta.com",
                Some("Delta Air Lines"),
                None,
                &[],
                &[],
                ranked(2_000, 5_000),
            ),
            Some("US"),
            &["airline"],
        );
        let (_dir, searcher) = build(&[wikipedia, grok, kayak, delta]);
        let hits = search_in(&searcher, "online encyclopedia", &options("US", false));
        assert_eq!(hits[..2], ["wikipedia.org", "grokipedia.com"], "{hits:?}");
        let hits = search_in(&searcher, "airlines", &options("US", false));
        assert_eq!(hits[0], "delta.com", "{hits:?}");
        assert!(names_kind(
            "Free Online Encyclopedias, by all",
            "onlineencyclopedia",
            2
        ));
        assert!(names_kind(
            "an online encyclopedia that anyone edits",
            "onlineencyclopedia",
            2
        ));
        assert!(!names_kind(
            "online and encyclopedia",
            "onlineencyclopedia",
            2
        ));
        assert!(!names_kind(
            "Search engine optimization tools",
            "searchengine",
            2
        ));
        assert!(names_kind("A private search engine.", "searchengine", 2));
    }

    #[test]
    fn strict_country_filter_keeps_global_sites() {
        let (_dir, searcher) = build(&bank_corpus());
        let only_us = search_in(&searcher, "banks", &options("US", true));
        assert!(
            only_us.iter().all(|d| !d.ends_with(".de") && d != "db.com"),
            "{only_us:?}"
        );
        assert!(only_us.contains(&"bankrate.com".to_string()));
        let hits = searcher
            .search_full(
                "deutsche bank",
                10,
                &RankConfig::default(),
                &options("US", true),
            )
            .unwrap()
            .hits;
        assert!(hits.iter().all(|hit| hit.country.as_deref() != Some("DE")));
        // Without a home country there is nothing to filter by.
        let hits = searcher
            .search_full(
                "deutsche bank",
                10,
                &RankConfig::default(),
                &SearchOptions {
                    country: None,
                    only_country: true,
                    exact: false,
                    ..SearchOptions::default()
                },
            )
            .unwrap()
            .hits;
        assert_eq!(hits[0].domain, "db.com");
        assert_eq!(hits[0].country.as_deref(), Some("DE"));
    }

    #[test]
    fn home_country_nudges_names_without_overruling_them() {
        let mut records = short_names_corpus();
        records.push(site(
            "bbc.co.uk",
            None,
            None,
            &["BBC"],
            &[],
            popular(100, 50_000),
        ));
        records.push(site(
            "bbc.com",
            None,
            None,
            &["BBC"],
            &[],
            popular(90, 50_000),
        ));
        let (_dir, searcher) = build(&records);
        let us = options("US", false);
        assert_eq!(search_in(&searcher, "american airlines", &us)[0], "aa.com");
        assert_eq!(
            search_in(&searcher, "bbc", &options("GB", false))[0],
            "bbc.co.uk"
        );
        assert_eq!(search_in(&searcher, "bbc", &us)[0], "bbc.com");
        // A French user asking for the French site by its name still gets it.
        let fr = search_in(&searcher, "americanairlines fr", &options("FR", false));
        assert_eq!(fr[0], "americanairlines.fr");
    }

    #[test]
    fn site_search_links_hand_off_the_rest_of_the_query() {
        let mut records = corpus();
        records.push(site(
            "github.com",
            Some("GitHub"),
            None,
            &["GitHub"],
            &[],
            popular(30, 100_000),
        ));
        let mut shop = site(
            "acme-shop.com",
            Some("Acme Shop"),
            None,
            &[],
            &[],
            ranked(5_000, 100),
        );
        shop.search_url = Some("https://www.acme-shop.com/find?q={searchTerms}".into());
        records.push(shop);
        let mut liar = site("liar.com", Some("Liar"), None, &[], &[], ranked(5_000, 100));
        liar.search_url = Some("https://evil.example/?q={searchTerms}".into());
        records.push(liar);
        let (_dir, searcher) = build(&records);
        let full = |q: &str| {
            searcher
                .search_full(q, 10, &RankConfig::default(), &SearchOptions::default())
                .unwrap()
        };

        let github = full("github SueHeir/plumb-search");
        assert_eq!(github.hits[0].domain, "github.com");
        assert_eq!(
            github.site_search,
            Some(SiteSearch {
                domain: "github.com".into(),
                terms: "SueHeir/plumb-search".into(),
                url: "https://github.com/search?q=SueHeir%2Fplumb-search".into(),
            })
        );
        let shop = full("acme shop  red  boots").site_search.unwrap();
        assert_eq!(shop.terms, "red boots");
        assert_eq!(shop.url, "https://www.acme-shop.com/find?q=red%20boots");
        // Just the name, a site without a search address, and a search
        // address on another site offer nothing.
        assert_eq!(full("github").site_search, None);
        assert_eq!(full("us bank login").site_search, None);
        assert_eq!(full("liar stuff").site_search, None);
    }

    #[test]
    fn safe_search_and_language_leave_sites_out() {
        let mut records = corpus();
        records.push(site(
            "bankporn.example",
            Some("Bank vault videos"),
            None,
            &[],
            &[],
            ranked(800, 5_000),
        ));
        let mut studio = site(
            "studio.example",
            Some("Bank Studio"),
            None,
            &[],
            &[],
            ranked(700, 5_000),
        );
        studio.kinds = vec!["pornographic film studio".into()];
        records.push(studio);
        records.push(site(
            "banklingerie.example",
            Some("Bank lingerie, sexy and simple"),
            None,
            &[],
            &[],
            ranked(750, 5_000),
        ));
        let mut german = site(
            "bankde.example",
            Some("Bank Deutschland"),
            None,
            &[],
            &[],
            ranked(760, 5_000),
        );
        german.language = Some("de".into());
        records.push(german);
        let (_dir, searcher) = build(&records);
        let with = |options: SearchOptions| {
            let hits = searcher
                .search_full("bank", 50, &RankConfig::default(), &options)
                .unwrap()
                .hits;
            domains(&hits)
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let has = |found: &[String], domain: &str| found.iter().any(|d| d == domain);

        let off = with(SearchOptions {
            safe: SafeSearch::Off,
            ..SearchOptions::default()
        });
        for domain in ["bankporn.example", "studio.example", "banklingerie.example"] {
            assert!(has(&off, domain), "{domain}");
        }
        let moderate = with(SearchOptions::default());
        assert!(!has(&moderate, "bankporn.example"));
        assert!(!has(&moderate, "studio.example"));
        assert!(has(&moderate, "banklingerie.example"));
        let strict = with(SearchOptions {
            safe: SafeSearch::Strict,
            ..SearchOptions::default()
        });
        assert!(!has(&strict, "banklingerie.example"));
        assert!(has(&strict, "usbank.com"));

        // Sites that say another language go; sites that say nothing stay.
        let english = with(SearchOptions {
            language: Some("en".into()),
            ..SearchOptions::default()
        });
        assert!(!has(&english, "bankde.example"));
        assert!(has(&english, "usbank.com"));
        let german = with(SearchOptions {
            language: Some("de".into()),
            ..SearchOptions::default()
        });
        assert!(has(&german, "bankde.example"));
    }

    #[test]
    fn search_operators_narrow_the_results() {
        let mut records = corpus();
        records.push(site(
            "github.com",
            Some("GitHub: where the world builds software"),
            None,
            &["GitHub"],
            &[],
            popular(30, 100_000),
        ));
        records.push(site(
            "bankrate.com",
            Some("Bankrate: mortgage rates and bank reviews"),
            None,
            &[],
            &[],
            ranked(900, 5_000),
        ));
        let (_dir, searcher) = build(&records);
        let full = |q: &str| {
            searcher
                .search_full(q, 10, &RankConfig::default(), &SearchOptions::default())
                .unwrap()
        };
        let plain = domains(&full("bank").hits).len();
        assert!(plain > 2);

        // Only sites under the named host, and the site itself.
        let on_site = full("bank site:bankrate.com");
        assert_eq!(domains(&on_site.hits), ["bankrate.com"]);
        // A site whose homepage lacks the words is still listed, with a
        // link into its own search.
        let github = full("site:github.com plumb \"search engine\"");
        assert_eq!(domains(&github.hits), ["github.com"]);
        assert_eq!(
            github.site_search.unwrap().url,
            "https://github.com/search?q=plumb%20%22search%20engine%22"
        );
        assert_eq!(domains(&full("site:github.com").hits), ["github.com"]);
        assert!(full("site:github.com").site_search.is_none());

        // Excluded words and hosts.
        let without = full("bank -bankrate");
        assert!(!domains(&without.hits).contains(&"bankrate.com"));
        assert!(!domains(&without.hits).is_empty());
        let not_site = full("bank -site:usbank.com");
        assert!(!domains(&not_site.hits).contains(&"usbank.com"));

        // A phrase keeps only sites with those words in that order.
        let phrase = full("\"bank reviews\"");
        assert!(domains(&phrase.hits).contains(&"bankrate.com"));
        assert!(!domains(&phrase.hits).contains(&"usbank.com"));
        assert!(full("-bank").hits.is_empty());
    }

    #[test]
    fn brand_names_find_the_official_site() {
        let (_dir, searcher) = build(&corpus());
        for query in [
            "us bank",
            "US Bank",
            "usbank",
            "u.s. bank",
            "U.S. Bank",
            "US BANK",
        ] {
            assert_eq!(top(&searcher, query), "usbank.com", "query {query:?}");
        }
        for query in ["bank of america", "bankofamerica", "Bank of America"] {
            assert_eq!(
                top(&searcher, query),
                "bankofamerica.com",
                "query {query:?}"
            );
        }
        assert_eq!(top(&searcher, "chase"), "chase.com");
        assert_eq!(top(&searcher, "wikipedia"), "wikipedia.org");
    }

    #[test]
    fn decoys_rank_below_the_brand() {
        let (_dir, searcher) = build(&corpus());
        let hits = searcher.search("us bank", 10).unwrap();
        let found = domains(&hits);
        assert_eq!(found[0], "usbank.com");
        // The decoys are still decent matches, just not first.
        assert!(found.contains(&"usbank-login-help.com"), "{found:?}");
        assert!(found.contains(&"usbankreviews.net"), "{found:?}");

        let hits = searcher.search("bank of america", 10).unwrap();
        let found = domains(&hits);
        assert_eq!(found[0], "bankofamerica.com");
        assert!(
            found.contains(&"bankofamerica-fraud-alerts.com"),
            "{found:?}"
        );
        assert!(found.contains(&"americabank.com"), "{found:?}");

        // Extra words still lead to the brand, not the page that has them all.
        assert_eq!(top(&searcher, "us bank login"), "usbank.com");
        // Word order matters: the small bank named exactly this beats the big one.
        assert_eq!(top(&searcher, "america bank"), "americabank.com");
    }

    #[test]
    fn stuffed_decoys_do_not_win() {
        let mut records = corpus();
        // Names itself "US Bank" wherever a site can: title, og:site_name, a few links.
        records.push(site(
            "usbank-online.com",
            Some("US Bank | US Bank Online | US Bank"),
            Some("US Bank US Bank US Bank online banking login."),
            &["US Bank"],
            &[("US Bank", 5), ("usbank", 2)],
            Signals::default(),
        ));
        // A fairly popular site whose whole name is one of the query words.
        records.push(site(
            "bank.com",
            Some("Bank.com"),
            None,
            &[],
            &[("bank", 40)],
            obscure(30_000, 500),
        ));
        let (_dir, searcher) = build(&records);
        for query in ["us bank", "usbank", "u.s. bank", "US Bank online banking"] {
            assert_eq!(top(&searcher, query), "usbank.com", "query {query:?}");
        }
        assert_eq!(top(&searcher, "bank"), "bank.com");
    }

    #[test]
    fn bare_names_spelling_the_query_lose_to_the_site_it_names() {
        let records = [
            site(
                "youtube.com",
                Some("YouTube"),
                Some("Enjoy the videos you love."),
                &["YouTube"],
                &[("youtube", 300)],
                popular(9, 6_000),
            ),
            // Never crawled, but well placed in the link graph.
            site(
                "you-tubemusic.com",
                None,
                None,
                &[],
                &[],
                obscure(300_000, 0),
            ),
            site(
                "ytmusicfans.net",
                Some("YouTube Music fans | YouTube Music playlists"),
                Some("The best YouTube Music playlists, picked by YouTube Music fans."),
                &[],
                &[("youtube music playlists", 4)],
                obscure(80_000, 3),
            ),
        ];
        let (_dir, searcher) = build(&records);
        let hits = searcher.search("youtube music", 10).unwrap();
        let lookalike = hits
            .iter()
            .find(|h| h.domain == "you-tubemusic.com")
            .unwrap();
        assert!(lookalike.link_score > RankConfig::default().trusted_link_score);
        assert_eq!(hits[0].domain, "youtube.com", "{:?}", domains(&hits));
        // With no other site named, the bare name is the answer.
        assert_eq!(top(&searcher, "you tubemusic"), "you-tubemusic.com");
    }

    #[test]
    fn sites_people_link_to_by_name_are_not_bare() {
        let records = [
            site(
                "home.com",
                Some("Home"),
                Some("Homes for sale."),
                &[],
                &[("home", 40)],
                obscure(5_000, 40),
            ),
            // Turns crawlers away, but people link to it by name.
            site(
                "homedepot.com",
                None,
                None,
                &[],
                &[("home depot", 200), ("the home depot", 80)],
                popular(60, 3_000),
            ),
            site(
                "homedepot.com.mx",
                Some("The Home Depot México"),
                Some("Home Depot: herramientas y materiales."),
                &[],
                &[],
                obscure(40_000, 2),
            ),
        ];
        let (_dir, searcher) = build(&records);
        let hits = searcher.search("home depot", 10).unwrap();
        assert_eq!(hits[0].domain, "homedepot.com", "{:?}", domains(&hits));
    }

    #[test]
    fn popularity_breaks_equal_name_matches() {
        let (_dir, searcher) = build(&corpus());
        let hits = searcher.search("acme", 10).unwrap();
        assert_eq!(domains(&hits)[..2], ["acme.com", "acme.net"]);
        assert_eq!(hits[0].text_score, hits[1].text_score);
        assert!(hits[0].link_score > hits[1].link_score);
    }

    #[test]
    fn link_text_alone_finds_a_site() {
        let (_dir, searcher) = build(&corpus());
        let hits = searcher.search("bofa", 10).unwrap();
        assert_eq!(hits[0].domain, "bankofamerica.com");
        let title = hits[0].title.as_deref().unwrap().to_lowercase();
        assert!(!title.contains("bofa"));
    }

    #[test]
    fn aliases_beat_bare_redirect_domains() {
        let (_dir, searcher) = build(&corpus());
        // allybank.com has the exact label, ally.com the alias, the page and the links.
        let hits = searcher.search("ally bank", 10).unwrap();
        assert_eq!(domains(&hits)[..2], ["ally.com", "allybank.com"]);
    }

    #[test]
    fn accents_and_case_do_not_matter() {
        let (_dir, searcher) = build(&corpus());
        for query in ["nestle", "Nestlé", "NESTLÉ", "nestlé"] {
            assert_eq!(top(&searcher, query), "nestle.com", "query {query:?}");
        }
    }

    #[test]
    fn punycode_labels_match_unicode_queries() {
        let (_dir, searcher) = build(&corpus());
        for query in ["bücher", "bucher", "Bücher"] {
            assert_eq!(top(&searcher, query), "xn--bcher-kva.de", "query {query:?}");
        }
    }

    #[test]
    fn typed_domains_go_to_that_domain() {
        let (_dir, searcher) = build(&corpus());
        for query in [
            "usbank.com",
            "www.usbank.com",
            "https://www.usbank.com/personal",
        ] {
            assert_eq!(top(&searcher, query), "usbank.com", "query {query:?}");
        }
        assert_eq!(
            top(&searcher, "usbank-login-help.com"),
            "usbank-login-help.com"
        );
        assert_eq!(top(&searcher, "bücher.de"), "xn--bcher-kva.de");
    }

    #[test]
    fn queries_without_letters_or_digits_find_nothing() {
        let (_dir, searcher) = build(&corpus());
        for query in ["", "   ", "!!!", "--- / ...", "'", "|", "\u{2019}"] {
            assert!(
                searcher.search(query, 10).unwrap().is_empty(),
                "query {query:?}"
            );
        }
        assert!(searcher.search("zzzzqqq", 10).unwrap().is_empty());
    }

    #[test]
    fn scores_blend_text_and_popularity() {
        let (_dir, searcher) = build(&corpus());
        let cfg = RankConfig::default();
        let check = |hits: &[Hit], expected: &dyn Fn(&Hit) -> f32| {
            assert!(hits.iter().any(|hit| hit.text_score == 1.0));
            for pair in hits.windows(2) {
                assert!(pair[0].score >= pair[1].score);
            }
            for hit in hits {
                assert!((0.0..=1.0).contains(&hit.text_score), "{hit:?}");
                assert!((0.0..=1.0).contains(&hit.link_score), "{hit:?}");
                assert!((hit.score - expected(hit)).abs() < 1e-5, "{hit:?}");
            }
        };

        // No site is named "online" or "online banking": a plain blend, at
        // the popularity weight of queries that describe what they look for,
        // with popularity counting in proportion below the relevance floor.
        let alpha = cfg.described_alpha.unwrap();
        let floor = cfg.described_relevance.unwrap();
        let hits = searcher.search("online banking", 10).unwrap();
        check(&hits, &|hit| {
            let prior = hit.link_score * (hit.text_score / floor).min(1.0);
            alpha * prior + (1.0 - alpha) * hit.text_score
        });

        // "us bank" is usbank.com's whole name: the label bonus, full trust.
        let hits = searcher.search("us bank", 10).unwrap();
        let usbank = &hits[0];
        assert_eq!(usbank.domain, "usbank.com");
        let record_score = corpus()[0].link_score();
        assert!((usbank.link_score - record_score).abs() < 1e-6);
        let label_bonus = |hit: &Hit, share: f32| {
            if hit.domain == "usbank.com" {
                cfg.exact_label_bonus * share
            } else {
                0.0
            }
        };
        check(&hits, &|hit| {
            cfg.alpha * hit.link_score + (1.0 - cfg.alpha) * hit.text_score + label_bonus(hit, 1.0)
        });

        // "us bank online" goes on past the name: usbank.com gets 2/3 of the
        // bonus, and sites below the trusted link score lose some of theirs.
        let hits = searcher.search("us bank online", 10).unwrap();
        assert_eq!(hits[0].domain, "usbank.com");
        let trusted = cfg.trusted_link_score.min(usbank.link_score);
        assert!(hits.iter().any(|hit| hit.link_score < trusted));
        check(&hits, &|hit| {
            let evidence = (hit.link_score / trusted).min(1.0);
            let trust = cfg.untrusted_share + (1.0 - cfg.untrusted_share) * evidence;
            cfg.alpha * hit.link_score
                + trust * ((1.0 - cfg.alpha) * hit.text_score + label_bonus(hit, 2.0 / 3.0))
        });
    }

    /// Official sites and the look-alikes built for their brand-plus-intent
    /// queries, modeled on `fixtures/`: keyword-stuffed titles, a link from
    /// one spam page, no rank.
    fn lookalike_corpus() -> Vec<SiteRecord> {
        let spam_linked = || Signals {
            linking_domains: 1,
            ..Signals::default()
        };
        vec![
            site(
                "irs.gov",
                Some("Internal Revenue Service (IRS) | An official website of the United States government"),
                Some("Find tax forms, check your refund status, make a payment and get answers to your tax questions."),
                &["IRS", "Internal Revenue Service"],
                &[("IRS", 2)],
                popular(410, 3),
            ),
            site(
                "irs-tax-refund-help.com",
                Some("IRS Tax Refund Help | Check IRS Refund Status | IRS Online"),
                Some("IRS refund status, IRS tax refund help and IRS online account support."),
                &[],
                &[("IRS Refund", 1)],
                spam_linked(),
            ),
            site(
                "usbank.com",
                Some("Personal and Business Banking | U.S. Bank"),
                None,
                &["U.S. Bank", "U.S. Bancorp"],
                &[("us bank", 2)],
                popular(503, 3),
            ),
            site(
                "usbank-login-help.com",
                Some("US Bank Login Help | US Bank Online Banking Sign In"),
                Some("US Bank login help: sign in to US Bank online banking."),
                &["US Bank Login Help"],
                &[("US Bank Login", 1)],
                spam_linked(),
            ),
            site(
                "amazon.com",
                Some("Amazon.com. Spend less. Smile more."),
                None,
                &["Amazon"],
                &[("Amazon", 2)],
                popular(5, 3),
            ),
            site(
                "amazon-prime-refund.com",
                Some("Amazon Prime Refund | Amazon Account Locked | Amazon Support"),
                Some("Your Amazon Prime refund is ready. Claim your Amazon refund."),
                &[],
                &[],
                Signals::default(),
            ),
            site(
                "chase.com",
                Some("Chase Bank - Credit Cards, Mortgages, Commercial Banking, Auto Loans"),
                None,
                &["Chase", "Chase Bank"],
                &[("Chase", 3)],
                popular(112, 4),
            ),
            site(
                "chasecenter.com",
                Some("Chase Center | San Francisco's Home of the Golden State Warriors"),
                None,
                &["Chase Center"],
                &[("Chase Center", 2)],
                popular(44_016, 2),
            ),
            // Little-known sites: only a link target, or nothing at all.
            site("github.com", None, None, &[], &[], spam_linked()),
            site(
                "maplestreetbakery.com",
                Some("Maple Street Bakery – Fresh Bread Daily"),
                None,
                &[],
                &[],
                Signals::default(),
            ),
            site(
                "panerabread.com",
                Some("Panera Bread | Bakery-Cafe | Order Online"),
                None,
                &["Panera Bread"],
                &[("Panera", 2)],
                popular(2_000, 50),
            ),
        ]
    }

    #[test]
    fn brand_plus_intent_queries_find_the_official_site() {
        let (_dir, searcher) = build(&lookalike_corpus());
        for (query, official, lookalike) in [
            ("irs refund", "irs.gov", "irs-tax-refund-help.com"),
            ("irs refund status", "irs.gov", "irs-tax-refund-help.com"),
            (
                "internal revenue service refund",
                "irs.gov",
                "irs-tax-refund-help.com",
            ),
            ("us bank login", "usbank.com", "usbank-login-help.com"),
            ("U.S. Bank login", "usbank.com", "usbank-login-help.com"),
            ("amazon refund", "amazon.com", "amazon-prime-refund.com"),
            // Even a domain that spells out the whole query.
            (
                "amazon prime refund",
                "amazon.com",
                "amazon-prime-refund.com",
            ),
            ("us bank login help", "usbank.com", "usbank-login-help.com"),
        ] {
            let hits = searcher.search(query, 10).unwrap();
            assert_eq!(hits[0].domain, official, "{query:?}: {hits:#?}");
            // The look-alike is still found, below the site it imitates.
            assert!(domains(&hits).contains(&lookalike), "{query:?}");
        }
        // Typing its hostname still goes to the look-alike.
        assert_eq!(
            top(&searcher, "usbank-login-help.com"),
            "usbank-login-help.com"
        );
        assert_eq!(
            top(&searcher, "amazon-prime-refund.com"),
            "amazon-prime-refund.com"
        );

        // Without the trust rule the stuffed titles win, which is what it is
        // for ("login" kept, as in an exact search).
        let untrusting = RankConfig {
            trusted_link_score: 0.0,
            ..RankConfig::default()
        };
        let exact = SearchOptions {
            exact: true,
            ..SearchOptions::default()
        };
        let hits = searcher
            .search_full("us bank login", 1, &untrusting, &exact)
            .unwrap()
            .hits;
        assert_eq!(hits[0].domain, "usbank-login-help.com");
    }

    #[test]
    fn little_known_sites_win_when_no_named_site_competes() {
        let (_dir, searcher) = build(&lookalike_corpus());
        // No rank either way: github.com is named by "github", and the
        // look-alikes stuffing "login" have no more evidence than it has.
        assert_eq!(top(&searcher, "github login"), "github.com");
        // No site is named "maple" or "maple street", so the popular bakery
        // does not discount the unranked one.
        let hits = searcher.search("maple street bakery", 10).unwrap();
        assert_eq!(
            domains(&hits)[..2],
            ["maplestreetbakery.com", "panerabread.com"]
        );
        assert_eq!(hits[0].link_score, 0.0);
    }

    #[test]
    fn intent_words_rank_the_site_they_follow() {
        let records = vec![
            site(
                "paypal.com",
                Some("PayPal: Pay, Send and Save Money"),
                None,
                &["PayPal"],
                &[("PayPal", 3)],
                popular(20, 3),
            ),
            // Some links of its own, so the trust rule alone lets it win.
            site(
                "paypal-login.us",
                Some("PayPal Login | Sign in to your PayPal account"),
                Some("PayPal login: sign in to PayPal."),
                &["PayPal Login"],
                &[("PayPal login", 2)],
                obscure(1_000_000, 40),
            ),
            site(
                "postgresql.org",
                Some("PostgreSQL: The world's most advanced open source database"),
                None,
                &["PostgreSQL"],
                &[("Postgres", 2)],
                popular(9_000, 3),
            ),
            // Named "postgres", but not what "postgres docs" is after.
            site(
                "postgres.ai",
                Some("Postgres.AI"),
                None,
                &[],
                &[],
                obscure(5_000_000, 2),
            ),
            site(
                "github.com",
                Some("GitHub: Let's build from here"),
                None,
                &["GitHub"],
                &[("docs", 3), ("GitHub", 3)],
                popular(30, 4),
            ),
            site(
                "readthedocs.org",
                Some("Read the Docs"),
                None,
                &["Read the Docs"],
                &[("Read the Docs", 2)],
                popular(3_000, 3),
            ),
        ];
        let (_dir, searcher) = build(&records);
        assert_eq!(top(&searcher, "paypal login"), "paypal.com");
        assert_eq!(top(&searcher, "PayPal sign in"), "paypal.com");
        // "postgres" is searched as typed, with a suggestion that finds
        // postgresql.org above postgres.ai.
        let results = searcher
            .search_full(
                "postgres docs",
                10,
                &RankConfig::default(),
                &SearchOptions::default(),
            )
            .unwrap();
        let fixed = results.spelling.expect("a suggestion").query;
        let hits = searcher.search(&fixed, 10).unwrap();
        let rank = |domain: &str| domains(&hits).iter().position(|&d| d == domain);
        let official = rank("postgresql.org").expect("postgresql.org");
        assert!(
            rank("postgres.ai").is_none_or(|other| official < other),
            "{fixed}: {hits:#?}"
        );
        // A well-known site named by all of it keeps the query.
        assert_eq!(top(&searcher, "read the docs"), "readthedocs.org");
        // Typing its hostname still goes to the look-alike.
        assert_eq!(top(&searcher, "paypal-login.us"), "paypal-login.us");
    }

    #[test]
    fn intent_words_are_cut_from_the_end() {
        for (query, name) in [
            ("paypal login", Some("paypal")),
            ("Bank of America sign in", Some("bank of america")),
            ("us bank login help", Some("us bank")),
            ("netflix help center", Some("netflix")),
            ("mdn web docs", Some("mdn")),
            ("login", None),
            ("help center", None),
            ("login help desk", None),
            ("chase center tickets", None),
        ] {
            assert_eq!(without_intent_words(query).as_deref(), name, "{query:?}");
        }
    }

    #[test]
    fn small_joining_words_name_no_site_in_a_longer_query() {
        let records = vec![
            site(
                "in.gov",
                Some("IN.gov | The Official Website of the State of Indiana"),
                None,
                &["Indiana"],
                &[("IN.gov", 3)],
                popular(300, 5_000),
            ),
            site(
                "denverpizzaco.com",
                Some("Denver Pizza Company | Pizza in Denver"),
                Some("Wood-fired pizza in Denver, Colorado."),
                &[],
                &[("Denver Pizza", 1)],
                obscure(10_000_000, 0),
            ),
        ];
        let (_dir, searcher) = build(&records);
        assert_eq!(top(&searcher, "pizza in denver"), "denverpizzaco.com");
        // On its own, the word is still a name.
        assert_eq!(top(&searcher, "in"), "in.gov");
    }

    #[test]
    fn longer_leading_names_win() {
        let (_dir, searcher) = build(&lookalike_corpus());
        // chasecenter.com covers "chase center", chase.com only "chase".
        let hits = searcher.search("chase center tickets", 10).unwrap();
        assert_eq!(domains(&hits)[..2], ["chasecenter.com", "chase.com"]);
        assert_eq!(top(&searcher, "chase login"), "chase.com");
    }

    #[test]
    fn named_sites_are_ranked_outside_the_bm25_candidates() {
        let (_dir, searcher) = build(&lookalike_corpus());
        let cfg = RankConfig::default();
        let full = searcher.search_with("us bank login", 10, &cfg).unwrap();
        // With one BM25 candidate, the stuffed look-alike, usbank.com still
        // gets scored because the query names it.
        let narrow = RankConfig {
            candidates: 1,
            ..RankConfig::default()
        };
        let hits = searcher.search_with("us bank login", 1, &narrow).unwrap();
        assert_eq!(domains(&hits), ["usbank.com"]);
        assert_eq!(full[0].domain, "usbank.com");
        // Scored one by one rather than in a batch, so equal up to rounding.
        assert!((hits[0].text_score - full[0].text_score).abs() < 1e-5);
        assert!((hits[0].score - full[0].score).abs() < 1e-5);
    }

    #[test]
    fn alpha_trades_text_for_popularity() {
        let (_dir, searcher) = build(&corpus());
        // Text only, no bonus: the page that best matches "bank" wins.
        let text_only = RankConfig {
            alpha: 0.0,
            exact_label_bonus: 0.0,
            exact_alias_bonus: 0.0,
            described_alpha: None,
            ..RankConfig::default()
        };
        let hits = searcher.search_with("bank", 20, &text_only).unwrap();
        assert_eq!(hits[0].text_score, 1.0);
        assert_eq!(hits[0].score, 1.0);
        // Popularity only: the most popular site matching "bank" wins.
        let prior_only = RankConfig {
            alpha: 1.0,
            ..text_only
        };
        let hits = searcher.search_with("bank", 20, &prior_only).unwrap();
        let best_link = hits.iter().map(|hit| hit.link_score).fold(0.0, f32::max);
        assert_eq!(hits[0].link_score, best_link);
        assert_eq!(hits[0].domain, "chase.com");
    }

    #[test]
    fn described_alpha_only_applies_when_no_site_is_named_in_full() {
        let (_dir, searcher) = build(&corpus());
        let cfg = RankConfig {
            alpha: 0.0,
            exact_label_bonus: 0.0,
            exact_alias_bonus: 0.0,
            described_alpha: Some(1.0),
            ..RankConfig::default()
        };
        // No site is named "bank": popularity alone decides.
        let hits = searcher.search_with("bank", 20, &cfg).unwrap();
        assert_eq!(hits[0].domain, "chase.com");
        assert_eq!(hits[0].score, hits[0].link_score);
        // usbank.com is named by all of "us bank": alpha stays 0.
        let hits = searcher.search_with("us bank", 20, &cfg).unwrap();
        assert_eq!(hits[0].domain, "usbank.com");
        assert_eq!(hits[0].score, hits[0].text_score);
    }

    #[test]
    fn described_queries_need_some_match_for_popularity_to_count() {
        let records = vec![
            site(
                "youtube.com",
                Some("YouTube"),
                Some("Enjoy the videos and music you love, and keep a watch list."),
                &["YouTube"],
                &[("YouTube", 50)],
                popular(1, 50),
            ),
            site(
                "checklist.com",
                Some("Checklist.com | To do list and checklists"),
                Some("Make a to do list and share checklists."),
                &[],
                &[("checklist", 1)],
                obscure(500_000, 2),
            ),
        ];
        let (_dir, searcher) = build(&records);
        // Popularity weighing heavily, as among a million sites, where
        // youtube.com matches "to do list" far less than here.
        let on = RankConfig {
            described_alpha: Some(0.9),
            described_relevance: Some(0.25),
            ..RankConfig::default()
        };
        let hits = searcher.search_with("to do list", 2, &on).unwrap();
        assert_eq!(hits[0].domain, "checklist.com");
        let off = RankConfig {
            described_relevance: None,
            ..on
        };
        let hits = searcher.search_with("to do list", 2, &off).unwrap();
        assert_eq!(hits[0].domain, "youtube.com");
    }

    #[test]
    fn partial_label_bonus_only_applies_to_part_of_the_query() {
        let (_dir, searcher) = build(&corpus());
        let no_partial = RankConfig {
            partial_label_bonus: None,
            ..RankConfig::default()
        };
        let full = searcher
            .search_with("us bank", 1, &RankConfig::default())
            .unwrap();
        let hits = searcher.search_with("us bank", 1, &no_partial).unwrap();
        assert_eq!(hits[0].domain, "usbank.com");
        assert!((hits[0].score - full[0].score).abs() < 1e-5);
    }

    #[test]
    fn limits_are_respected() {
        let (_dir, searcher) = build(&corpus());
        assert_eq!(searcher.search("bank", 2).unwrap().len(), 2);
        assert!(searcher.search("bank", 0).unwrap().is_empty());
        let all = searcher.search("bank", 1000).unwrap();
        assert!(all.len() <= searcher.num_docs() as usize);
        let few_candidates = RankConfig {
            candidates: 0,
            ..RankConfig::default()
        };
        let hits = searcher.search_with("us bank", 3, &few_candidates).unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].domain, "usbank.com");
    }

    #[test]
    fn hits_carry_page_fields_and_url_fallback() {
        let (_dir, searcher) = build(&corpus());
        let hit = &searcher.search("us bank", 1).unwrap()[0];
        assert_eq!(hit.url, "https://www.usbank.com/");
        assert_eq!(
            hit.title.as_deref(),
            Some("U.S. Bank | Personal Banking, Credit Cards, Home Loans & More")
        );
        assert!(hit
            .description
            .as_deref()
            .unwrap()
            .starts_with("Discover U.S. Bank"));

        let hits = searcher.search("acme", 10).unwrap();
        let acme_net = hits.iter().find(|hit| hit.domain == "acme.net").unwrap();
        assert_eq!(acme_net.url, "https://acme.net/");
        assert_eq!(acme_net.description, None);
    }

    /// The names in `dir`, sorted, hidden staging leftovers included.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn example_site() -> SiteRecord {
        site(
            "example.com",
            Some("Example Domain"),
            None,
            &[],
            &[],
            Signals::default(),
        )
    }

    #[test]
    fn hits_carry_the_site_key_pages() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("index");
        let mut site = example_site();
        site.key_pages = vec![
            KeyPage {
                label: "Docs".into(),
                url: "https://docs.example.com/".into(),
            },
            KeyPage {
                label: "Elsewhere".into(),
                url: "https://other.example/".into(),
            },
        ];
        build_index(&dir, &[site]).unwrap();
        let searcher = Searcher::open(&dir).unwrap();
        let hits = searcher.search("example", 10).unwrap();
        let labels: Vec<&str> = hits[0].key_pages.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["Docs"], "only pages on the site itself");
    }

    /// Makes builds on this thread fail at `step` while it lives.
    struct FailAt;

    impl FailAt {
        fn step(step: &'static str) -> FailAt {
            FAIL_AT.set(Some(step));
            FailAt
        }
    }

    impl Drop for FailAt {
        fn drop(&mut self) {
            FAIL_AT.set(None);
        }
    }

    #[test]
    fn rebuilding_replaces_the_old_index() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("index");
        build_index(&dir, &corpus()).unwrap();
        let old = Searcher::open(&dir).unwrap();
        assert_eq!(old.num_docs(), corpus().len() as u64);

        let stats = build_index(&dir, &[example_site()]).unwrap();
        assert_eq!(stats.docs, 1);
        let new = Searcher::open(&dir).unwrap();
        assert_eq!(new.num_docs(), 1);
        assert!(new.search("us bank", 10).unwrap().is_empty());
        assert_eq!(top(&new, "example"), "example.com");
        assert_eq!(entries(root.path()), ["index"]);
        assert!(entries(&dir).iter().any(|name| name == replace::MARKER));
        // An already open searcher keeps serving the index it opened (on Unix,
        // where deleted files stay readable while open).
        if cfg!(unix) {
            assert_eq!(top(&old, "us bank"), "usbank.com");
        }
    }

    #[test]
    fn failed_builds_keep_the_previous_index() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("index");
        build_index(&dir, &corpus()).unwrap();

        // Fails after the new index was committed in its staging directory.
        let failing = FailAt::step("after_commit");
        let err = build_index(&dir, &[example_site()]).unwrap_err();
        drop(failing);
        assert!(format!("{err:#}").contains("injected failure"), "{err:#}");

        // The previous index is still there and searchable, with no leftovers.
        let searcher = Searcher::open(&dir).unwrap();
        assert_eq!(searcher.num_docs(), corpus().len() as u64);
        assert_eq!(top(&searcher, "us bank"), "usbank.com");
        assert_eq!(entries(root.path()), ["index"]);

        // The next build goes through.
        build_index(&dir, &[example_site()]).unwrap();
        let searcher = Searcher::open(&dir).unwrap();
        assert_eq!(top(&searcher, "example"), "example.com");
        assert_eq!(entries(root.path()), ["index"]);
    }

    #[test]
    fn refuses_to_replace_a_directory_that_is_not_an_index() {
        let root = TempDir::new().unwrap();
        let data = root.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("notes.txt"), "keep me").unwrap();
        let err = build_index(&data, &corpus()).unwrap_err();
        assert!(err.to_string().contains("refusing"), "{err:#}");
        assert_eq!(
            fs::read_to_string(data.join("notes.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(entries(root.path()), ["data"]);

        // A missing directory is created, an empty one is used.
        let missing = root.path().join("a/b/index");
        assert_eq!(build_index(&missing, &[] as &[SiteRecord]).unwrap().docs, 0);
        assert_eq!(Searcher::open(&missing).unwrap().num_docs(), 0);
        let empty = root.path().join("empty");
        fs::create_dir(&empty).unwrap();
        assert_eq!(build_index(&empty, &corpus()[..2]).unwrap().docs, 2);
        assert_eq!(entries(root.path()), ["a", "data", "empty"]);
    }

    /// The reported case: `plumb index --index myproject` deleted a project
    /// folder because it held a file named `meta.json`.
    #[test]
    fn keeps_a_project_folder_that_holds_a_meta_json() {
        let root = TempDir::new().unwrap();
        // Even a `meta.json` copied from a real index does not make one.
        build_index(&root.path().join("real"), &[example_site()]).unwrap();
        let tantivy_meta = fs::read_to_string(root.path().join("real/meta.json")).unwrap();
        fs::remove_dir_all(root.path().join("real")).unwrap();

        for meta in [
            "{}",
            r#"{"name": "myproject", "version": "1.0.0"}"#,
            &tantivy_meta,
        ] {
            let project = root.path().join("myproject");
            fs::create_dir_all(project.join("src")).unwrap();
            fs::write(project.join("meta.json"), meta).unwrap();
            fs::write(project.join("notes.txt"), "my notes").unwrap();
            fs::write(project.join("src/main.rs"), "fn main() {}").unwrap();

            let err = format!("{:#}", build_index(&project, &corpus()).unwrap_err());
            assert!(err.contains("refusing to replace"), "{err}");
            assert!(err.contains("myproject"), "{err}");
            assert_eq!(entries(&project), ["meta.json", "notes.txt", "src"]);
            assert_eq!(fs::read_to_string(project.join("meta.json")).unwrap(), meta);
            assert_eq!(
                fs::read_to_string(project.join("notes.txt")).unwrap(),
                "my notes"
            );
            assert_eq!(
                fs::read_to_string(project.join("src/main.rs")).unwrap(),
                "fn main() {}"
            );
            assert_eq!(entries(root.path()), ["myproject"]);
            fs::remove_dir_all(&project).unwrap();
        }
    }

    #[test]
    fn indexes_built_before_the_marker_are_replaced() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("index");
        build_index(&dir, &corpus()).unwrap();
        fs::remove_file(dir.join(replace::MARKER)).unwrap();

        build_index(&dir, &[example_site()]).unwrap();
        let searcher = Searcher::open(&dir).unwrap();
        assert_eq!(top(&searcher, "example"), "example.com");
        assert!(entries(&dir).iter().any(|name| name == replace::MARKER));

        // Not when it holds anything besides the index.
        fs::remove_file(dir.join(replace::MARKER)).unwrap();
        fs::write(dir.join("notes.txt"), "keep me").unwrap();
        let err = format!("{:#}", build_index(&dir, &corpus()).unwrap_err());
        assert!(err.contains("it holds notes.txt"), "{err}");
        assert_eq!(Searcher::open(&dir).unwrap().num_docs(), 1);
        assert_eq!(
            fs::read_to_string(dir.join("notes.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(entries(root.path()), ["index"]);
    }

    #[test]
    fn records_without_a_valid_domain_are_skipped() {
        let dir = TempDir::new().unwrap();
        let records = [
            SiteRecord::new("  "),
            SiteRecord::new("not a domain"),
            SiteRecord::new("localhost"),
            SiteRecord::new("192.168.1.1"),
            SiteRecord::new("co.uk"),
            SiteRecord::new("Example.COM"),
        ];
        let stats = build_index(dir.path(), &records).unwrap();
        let expected = IndexStats {
            docs: 1,
            merged: 0,
            skipped: 5,
            redirected: 0,
        };
        assert_eq!(stats, expected);
        let searcher = Searcher::open(dir.path()).unwrap();
        assert_eq!(searcher.num_docs(), 1);
        assert_eq!(top(&searcher, "example"), "example.com");
    }

    #[test]
    fn records_for_the_same_domain_are_merged() {
        let mut upper = site(
            "Example.com",
            Some("Example Domain"),
            None,
            &["Example"],
            &[("example", 5)],
            obscure(50_000, 10),
        );
        upper.crawled_at = Some(100);
        let mut lower = site(
            "www.example.com.",
            Some("Old Example Title"),
            Some("For use in illustrative examples."),
            &["Example Inc"],
            &[("example", 3), ("illustrative examples", 2)],
            popular(900, 20),
        );
        lower.url = Some("https://example.com/".into());
        let unicode = site(
            "münchen.de",
            Some("Landeshauptstadt München"),
            None,
            &[],
            &[],
            obscure(80_000, 30),
        );
        let punycode = site(
            "xn--mnchen-3ya.de",
            None,
            Some("Das offizielle Stadtportal"),
            &[],
            &[("stadtportal", 4)],
            Signals::default(),
        );
        let records = [upper.clone(), unicode, lower.clone(), punycode];
        let dir = TempDir::new().unwrap();
        let stats = build_index(dir.path(), &records).unwrap();
        let expected = IndexStats {
            docs: 2,
            merged: 2,
            skipped: 0,
            redirected: 0,
        };
        assert_eq!(stats, expected);

        let searcher = Searcher::open(dir.path()).unwrap();
        assert_eq!(searcher.num_docs(), 2);
        let hits = searcher.search("example", 10).unwrap();
        assert_eq!(domains(&hits), ["example.com"]);
        // Merged as `SiteRecord::merge` does: the crawled record's page
        // fields first, the rest filled in, the best signals of both.
        let mut merged = upper;
        merged.domain = "example.com".into();
        lower.domain = "example.com".into();
        merged.merge(lower);
        assert_eq!(hits[0].title.as_deref(), Some("Example Domain"));
        assert_eq!(
            hits[0].description.as_deref(),
            Some("For use in illustrative examples.")
        );
        assert_eq!(hits[0].url, "https://example.com/");
        assert_eq!(hits[0].link_score, merged.link_score());
        assert_eq!(top(&searcher, "illustrative examples"), "example.com");
        assert_eq!(top(&searcher, "example inc"), "example.com");

        // The Unicode and punycode spellings are one site.
        for query in ["münchen", "munchen", "stadtportal", "münchen.de"] {
            let hits = searcher.search(query, 10).unwrap();
            assert_eq!(domains(&hits), ["xn--mnchen-3ya.de"], "query {query:?}");
            assert_eq!(hits[0].title.as_deref(), Some("Landeshauptstadt München"));
            assert_eq!(
                hits[0].description.as_deref(),
                Some("Das offizielle Stadtportal")
            );
        }
    }

    #[test]
    fn dotted_capital_i_is_found_by_plain_i() {
        // Lowercasing `İ` gives `i` plus a combining dot.
        let mut airport = SiteRecord::new("istairport.com");
        airport.add_link_text("İstanbul Havalimanı", "havaist.com");
        airport.add_link_text("İstanbul Havalimanı", "turkishairlines.com");
        let mut records = corpus();
        records.push(airport);
        let (_dir, searcher) = build(&records);
        for query in ["istanbul", "İstanbul", "ISTANBUL", "istanbul havalimanı"] {
            assert_eq!(top(&searcher, query), "istairport.com", "query {query:?}");
        }
    }

    #[test]
    fn opening_a_missing_index_fails() {
        let dir = TempDir::new().unwrap();
        assert!(Searcher::open(&dir.path().join("nope")).is_err());
        assert!(Searcher::open(dir.path()).is_err());
    }

    #[test]
    fn empty_index_finds_nothing() {
        let (_dir, searcher) = build(&[]);
        assert_eq!(searcher.num_docs(), 0);
        assert!(searcher.search("us bank", 10).unwrap().is_empty());
    }

    #[test]
    fn unsorted_and_oversized_records_are_tamed() {
        // Records read from JSON need not respect SiteRecord's own caps.
        let mut record = SiteRecord::new("example.com");
        record.title = Some(format!("Example {}", "very long title ".repeat(100)));
        record.link_texts = (1..=60)
            .map(|i| LinkText::with_count(format!("anchor {i}"), i))
            .collect();
        record.aliases = (100..140).map(|i| format!("alias {i}")).collect();
        let (_dir, searcher) = build(&[record]);
        let hit = &searcher.search("example", 1).unwrap()[0];
        assert!(hit.title.as_deref().unwrap().chars().count() <= MAX_TEXT_CHARS);
        let found = |query: &str| !searcher.search(query, 1).unwrap().is_empty();
        // The 32 most frequent link texts are kept (counts 60 down to 29).
        assert!(found("60") && found("29"));
        assert!(!found("28") && !found("1"));
        // The first 16 aliases are kept.
        assert!(found("100") && found("115"));
        assert!(!found("116"));
    }

    #[test]
    fn searcher_is_shared_between_threads() {
        let (_dir, searcher) = build(&corpus());
        let searcher = Arc::new(searcher);
        let handles: Vec<_> = ["us bank", "bank of america", "chase", "nestle"]
            .into_iter()
            .map(|query| {
                let searcher = Arc::clone(&searcher);
                std::thread::spawn(move || top(&searcher, query))
            })
            .collect();
        let tops: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            tops,
            ["usbank.com", "bankofamerica.com", "chase.com", "nestle.com"]
        );
    }

    #[test]
    fn queries_are_parsed_like_indexed_text() {
        let words = analysis::words_analyzer();
        let joined = analysis::joined_analyzer();
        let parse = |q: &str| ParsedQuery::new(q, &words, &joined);
        assert_eq!(
            parse("U.S. Bank").unwrap(),
            ParsedQuery {
                words: vec!["us".into(), "bank".into()],
                others: vec![None, Some("banks".into())],
                joined: Some("usbank".into()),
                leading: vec![("us".into(), 1), ("usbank".into(), 2)],
                kind: Some("usbank".into()),
                len: 2,
                domain: None,
            }
        );
        let keys = |parsed: ParsedQuery| -> Vec<String> {
            parsed.leading.into_iter().map(|(key, _)| key).collect()
        };
        let parsed = parse("bank bank BANK").unwrap();
        assert_eq!(parsed.words, ["bank"]);
        assert_eq!(parsed.joined.as_deref(), Some("bankbankbank"));
        assert_eq!(parsed.len, 3);
        assert_eq!(keys(parsed), ["bank", "bankbank", "bankbankbank"]);
        // Names are compared as indexed: folded, lowercased, punctuation gone.
        assert_eq!(
            keys(parse("Nestlé S.A. login").unwrap()),
            ["nestle", "nestlesa", "nestlesalogin"]
        );
        // A leading "the" may be left out; the words it covers still count it.
        assert_eq!(
            parse("The New York Times").unwrap().leading.last(),
            Some(&("newyorktimes".to_string(), 4))
        );
        assert_eq!(parse("Banks").unwrap().kind.as_deref(), Some("bank"));
        assert_eq!(
            parse("credit unions").unwrap().kind.as_deref(),
            Some("creditunion")
        );
        assert_eq!(
            parse("https://www.usbank.com/").unwrap().domain.as_deref(),
            Some("usbank.com")
        );
        assert_eq!(parse("u.s. bank").unwrap().domain, None);
        assert_eq!(parse("!!!"), None);
        let many = (0..40)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let many = parse(&many).unwrap();
        assert_eq!(many.words.len(), MAX_QUERY_WORDS);
        assert_eq!(many.leading.len(), MAX_QUERY_WORDS);
        assert_eq!(many.len, 40);
    }

    #[test]
    fn partial_configs_deserialize_with_defaults() {
        let cfg: RankConfig = serde_json_like("alpha", 0.5);
        assert_eq!(cfg.alpha, 0.5);
        assert_eq!(
            cfg,
            RankConfig {
                alpha: 0.5,
                ..RankConfig::default()
            }
        );
        let cfg: RankConfig = serde_json_like("trusted_link_score", 0.3);
        assert_eq!(cfg.trusted_link_score, 0.3);
        assert_eq!(cfg.untrusted_share, RankConfig::default().untrusted_share);
    }

    #[test]
    fn out_of_range_trust_settings_are_tamed() {
        let (_dir, searcher) = build(&lookalike_corpus());
        let expected = searcher.search("us bank login", 10).unwrap();
        for (trusted_link_score, untrusted_share) in [(f32::NAN, f32::NAN), (5.0, 0.5)] {
            let cfg = RankConfig {
                trusted_link_score,
                untrusted_share,
                ..RankConfig::default()
            };
            let hits = searcher.search_with("us bank login", 10, &cfg).unwrap();
            assert_eq!(domains(&hits)[0], "usbank.com");
            assert!(hits.iter().all(|hit| hit.score.is_finite()));
        }
        // NaN falls back to the defaults.
        let nan = RankConfig {
            trusted_link_score: f32::NAN,
            untrusted_share: f32::NAN,
            ..RankConfig::default()
        };
        assert_eq!(
            searcher.search_with("us bank login", 10, &nan).unwrap(),
            expected
        );
    }

    /// Deserializes a one-field map, without pulling in a JSON crate.
    fn serde_json_like(key: &str, value: f32) -> RankConfig {
        use serde::de::value::{Error, MapDeserializer};
        let map = MapDeserializer::<_, Error>::new(std::iter::once((key, value)));
        RankConfig::deserialize(map).unwrap()
    }

    /// Deterministic xorshift generator for synthetic records.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        fn word(&mut self) -> &'static str {
            const WORDS: [&str; 48] = [
                "bank", "credit", "union", "home", "loans", "city", "first", "national", "news",
                "shop", "online", "travel", "health", "care", "auto", "parts", "food", "pizza",
                "best", "deals", "tech", "cloud", "data", "music", "games", "sports", "books",
                "photo", "art", "design", "law", "firm", "real", "estate", "school", "college",
                "church", "garden", "pet", "store", "hotel", "golf", "club", "media", "radio",
                "farm", "energy", "america",
            ];
            WORDS[self.below(WORDS.len() as u64) as usize]
        }
    }

    fn synthetic_records(n: usize) -> Vec<SiteRecord> {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        (0..n)
            .map(|i| {
                let (a, b, c) = (rng.word(), rng.word(), rng.word());
                let mut record = SiteRecord::new(format!("{a}{b}{i}.com"));
                record.title = Some(format!("{a} {b} {i} | {c} {}", rng.word()));
                record.description =
                    Some((0..12).map(|_| rng.word()).collect::<Vec<_>>().join(" "));
                for _ in 0..rng.below(5) {
                    let text = format!("{} {}", rng.word(), rng.word());
                    record
                        .link_texts
                        .push(LinkText::with_count(text, 1 + rng.below(200) as u32));
                }
                if rng.below(4) == 0 {
                    record.add_alias(&format!("{a} {b}"));
                }
                record.signals.harmonic_rank = Some(1_000 + rng.below(50_000_000));
                record.signals.linking_domains = rng.below(5_000) as u32;
                record
            })
            .collect()
    }

    /// About 100k records; slow in debug builds, so run it with
    /// `cargo test -p plumb-index --release -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn builds_100k_records_in_reasonable_time() {
        let mut records = synthetic_records(100_000);
        records.extend(corpus());
        let dir = TempDir::new().unwrap();

        let start = Instant::now();
        let stats = build_index(dir.path(), &records).unwrap();
        let build_time = start.elapsed();
        assert_eq!(stats.docs, records.len() as u64);

        let searcher = Searcher::open(dir.path()).unwrap();
        assert_eq!(searcher.num_docs(), stats.docs);
        let queries = [
            "us bank",
            "bank of america",
            "bofa",
            "chase",
            "nestle",
            "acme",
        ];
        let expected = [
            "usbank.com",
            "bankofamerica.com",
            "bankofamerica.com",
            "chase.com",
            "nestle.com",
            "acme.com",
        ];
        let start = Instant::now();
        let rounds = 50;
        for _ in 0..rounds {
            for (query, domain) in queries.iter().zip(expected) {
                assert_eq!(top(&searcher, query), domain, "query {query:?}");
            }
        }
        let per_query = start.elapsed() / (rounds * queries.len() as u32);
        eprintln!(
            "built {} docs in {build_time:.2?}; {per_query:.2?} per query",
            stats.docs
        );
        assert!(build_time.as_secs() < 300, "build took {build_time:?}");
    }

    /// Well-known sites, an obscure look-alike, and enough sites about
    /// pizza and forecasts that the index knows those words.
    fn typo_corpus() -> Vec<SiteRecord> {
        let mut records = vec![
            site(
                "google.com",
                Some("Google"),
                None,
                &[],
                &[],
                popular(1, 90_000),
            ),
            site(
                "amazon.com",
                Some("Amazon.com. Spend less. Smile more."),
                None,
                &["Amazon"],
                &[("amazon prime", 300)],
                popular(10, 50_000),
            ),
            site(
                "bankofamerica.com",
                Some("Bank of America - Banking, Credit Cards, Loans"),
                None,
                &["Bank of America"],
                &[],
                popular(300, 20_000),
            ),
            site(
                "weather.com",
                Some("The Weather Channel"),
                None,
                &[],
                &[("weather forecast", 200)],
                popular(150, 30_000),
            ),
            site(
                "piazza.com",
                Some("Piazza: ask questions in class"),
                None,
                &[],
                &[],
                popular(4_000, 3_000),
            ),
            site(
                "gogle.com",
                Some("Gogle - free prizes"),
                None,
                &[],
                &[],
                obscure(40_000_000, 1),
            ),
            site(
                "amazen.com",
                Some("Amazen deals"),
                None,
                &[],
                &[],
                obscure(30_000_000, 2),
            ),
        ];
        records.extend([
            site(
                "hilton.com",
                Some("Hilton"),
                None,
                &[],
                &[],
                popular(900, 15_000),
            ),
            site(
                "hilten.com",
                Some("Hilten"),
                None,
                &[],
                &[],
                ranked(40_000, 2_000),
            ),
            site(
                "capitalone.com",
                Some("Capital One"),
                None,
                &["Capital One"],
                &[],
                popular(200, 20_000),
            ),
            site(
                "tacobell.com",
                Some("Taco Bell"),
                None,
                &["Taco Bell"],
                &[],
                popular(1_500, 10_000),
            ),
            site(
                "fedex.com",
                Some("FedEx"),
                None,
                &[],
                &[],
                popular(400, 20_000),
            ),
            site("edx.org", Some("edX"), None, &[], &[], popular(300, 30_000)),
        ]);
        for i in 0..25 {
            records.push(site(
                &format!("capitol{i}.gov"),
                Some(&format!("Capitol office {i}")),
                None,
                &[],
                &[],
                obscure(3_000_000 + i, 10),
            ));
            records.push(site(
                &format!("taco{i}.com"),
                Some(&format!("Taco stand {i}")),
                None,
                &[],
                &[],
                obscure(4_000_000 + i, 10),
            ));
        }
        for i in 0..25 {
            records.push(site(
                &format!("pizzeria{i}.com"),
                Some(&format!("Pizza place number {i}")),
                None,
                &[],
                &[],
                obscure(1_000_000 + i, 10),
            ));
        }
        for i in 0..5 {
            records.push(site(
                &format!("forecaster{i}.com"),
                Some("Local forecast"),
                None,
                &[],
                &[],
                obscure(2_000_000 + i, 10),
            ));
        }
        records
    }

    fn search_spelled(searcher: &Searcher, query: &str) -> SearchResults {
        searcher
            .search_full(query, 10, &RankConfig::default(), &SearchOptions::default())
            .unwrap()
    }

    fn suggested(query: &str) -> Option<Spelling> {
        Some(Spelling {
            query: query.to_string(),
        })
    }

    #[test]
    fn misspelled_names_find_the_site() {
        let (_dir, searcher) = build(&typo_corpus());
        for (typed, fixed, domain) in [
            ("amazom", "amazon", "amazon.com"),
            ("Amazn", "amazon", "amazon.com"),
            ("gooogle", "google", "google.com"),
            ("bank of amercia", "bank of america", "bankofamerica.com"),
            ("bank of americ", "bank of america", "bankofamerica.com"),
            ("weather forcast", "weather forecast", "weather.com"),
        ] {
            let results = search_spelled(&searcher, typed);
            assert_eq!(results.spelling, suggested(fixed), "{typed}");
            // The suggestion finds the site.
            let fixed_hits = search_spelled(&searcher, fixed).hits;
            assert_eq!(fixed_hits[0].domain, domain, "{typed}");
        }
    }

    #[test]
    fn typos_never_lead_to_look_alikes() {
        let (_dir, searcher) = build(&typo_corpus());
        // amazen.com is as near "amazn" as amazon.com, but has nothing to
        // show for itself.
        let results = search_spelled(&searcher, "amazn");
        assert_eq!(results.spelling, suggested("amazon"));
        let fixed_hits = search_spelled(&searcher, "amazon").hits;
        assert_eq!(domains(&fixed_hits)[0], "amazon.com");
    }

    #[test]
    fn exact_names_are_not_corrected() {
        let (_dir, searcher) = build(&typo_corpus());
        for query in [
            "amazon",
            "google",
            "bank of america",
            "piazza",
            "amazon.com",
        ] {
            let results = search_spelled(&searcher, query);
            assert_eq!(results.spelling, None, "{query}");
        }
        // A little-known site named exactly is searched as typed, with
        // the far better-known site a letter away offered instead.
        let results = search_spelled(&searcher, "gogle");
        assert_eq!(results.spelling, suggested("google"));
        assert_eq!(results.hits[0].domain, "gogle.com");
        // Searching exactly finds it with no suggestion.
        let options = SearchOptions {
            exact: true,
            ..SearchOptions::default()
        };
        let hits = searcher
            .search_full("gogle", 10, &RankConfig::default(), &options)
            .unwrap()
            .hits;
        assert_eq!(hits[0].domain, "gogle.com");
        // A site with popularity of its own keeps its name, though a more
        // popular one is a letter away.
        let results = search_spelled(&searcher, "hilten");
        assert_eq!(results.spelling, None);
        assert_eq!(results.hits[0].domain, "hilten.com");
    }

    #[test]
    fn a_site_named_by_every_word_keeps_its_name() {
        let mut records = typo_corpus();
        records.extend([
            site(
                "linux.org",
                Some("Linux"),
                None,
                &["Linux"],
                &[],
                popular(500, 20_000),
            ),
            site(
                "linustechtips.com",
                Some("Forums - Linus Tech Tips"),
                None,
                &[],
                &[],
                obscure(900_000, 30),
            ),
        ]);
        for i in 0..25 {
            records.push(site(
                &format!("linux{i}.com"),
                Some(&format!("Linux tips {i}")),
                None,
                &[],
                &[],
                obscure(5_000_000 + i, 10),
            ));
        }
        let (_dir, searcher) = build(&records);
        // "linus" is a letter from the well-known "linux", but the whole
        // query names a site: only its first word would be corrected.
        let results = search_spelled(&searcher, "linus tech tips");
        assert_eq!(results.spelling, None);
        assert_eq!(results.hits[0].domain, "linustechtips.com");
    }

    #[test]
    fn several_word_names_and_first_letters() {
        let (_dir, searcher) = build(&typo_corpus());
        for (typed, fixed, domain) in [
            // "capitol" and "taco" are words the index knows, and "bel" is
            // short, but together they are a typo of a well-known name.
            ("capitol one", "capital one", "capitalone.com"),
            ("taco bel", "taco bell", "tacobell.com"),
            // edx.org and fedex.com are both an edit away; typos rarely
            // change the first letter.
            ("fedx", "fedex", "fedex.com"),
        ] {
            let results = search_spelled(&searcher, typed);
            assert_eq!(results.spelling, suggested(fixed), "{typed}");
            // The suggestion finds the site.
            let fixed_hits = search_spelled(&searcher, fixed).hits;
            assert_eq!(fixed_hits[0].domain, domain, "{typed}");
        }
    }

    #[test]
    fn known_words_and_short_words_are_left_alone() {
        let (_dir, searcher) = build(&typo_corpus());
        // piazza.com is a letter away, but pizza is a word the index knows.
        let results = search_spelled(&searcher, "pizza");
        assert_eq!(results.spelling, None);
        assert!(results.hits[0].domain.starts_with("pizzeria"));
        // Too short to tell a typo from another name.
        assert_eq!(search_spelled(&searcher, "gle").spelling, None);
    }

    #[test]
    fn exact_searches_skip_correction() {
        let (_dir, searcher) = build(&typo_corpus());
        let options = SearchOptions {
            exact: true,
            ..SearchOptions::default()
        };
        let results = searcher
            .search_full("amazom", 10, &RankConfig::default(), &options)
            .unwrap();
        assert_eq!(results.spelling, None);
        assert!(!domains(&results.hits).contains(&"amazon.com"));
    }
    #[test]
    fn plurals_find_singulars_but_never_outweigh_the_words_typed() {
        let mut records = vec![
            site(
                "vimeo.com",
                Some("Vimeo"),
                Some("The all-in-one video platform"),
                &[],
                &[],
                ranked(150, 40_000),
            ),
            site(
                "clips.example",
                Some("Clips"),
                Some("Short videos"),
                &[],
                &[],
                ranked(150, 40_000),
            ),
        ];
        // Many sites have "videos", so "video" is the rarer word.
        for i in 0..30 {
            records.push(site(
                &format!("v{i}.example"),
                Some("Free videos"),
                Some("Free videos to watch"),
                &[],
                &[],
                obscure(5_000_000 + i, 2),
            ));
        }
        let (_dir, searcher) = build(&records);
        let hits = searcher.search("videos", 40).unwrap();
        let rank = |domain: &str| domains(&hits).iter().position(|d| *d == domain);
        let (Some(vimeo), Some(clips)) = (rank("vimeo.com"), rank("clips.example")) else {
            panic!("{hits:?}");
        };
        assert!(clips < vimeo, "{hits:?}");
    }

    #[test]
    fn the_most_popular_matches_are_ranked_past_the_bm25_cutoff() {
        let mut youtube = site(
            "youtube.com",
            Some("YouTube"),
            Some("Enjoy the videos and music you love, and share it all with the world"),
            &[],
            &[],
            popular(2, 2_000_000),
        );
        youtube.about = Some("American online video sharing platform".into());
        let mut records = vec![youtube];
        for i in 0..30 {
            records.push(site(
                &format!("watch{i}.example"),
                Some("Watch videos online"),
                Some("Watch free videos online"),
                &[],
                &[],
                obscure(5_000_000 + i, 2),
            ));
        }
        let (_dir, searcher) = build(&records);
        let cfg = RankConfig {
            candidates: 10,
            ..RankConfig::default()
        };
        let hits = searcher
            .search_with("watch videos online", 10, &cfg)
            .unwrap();
        assert!(domains(&hits).contains(&"youtube.com"), "{hits:?}");
    }
    #[test]
    fn popular_sites_a_little_further_in_meaning_are_ranked() {
        let mut records = vec![site(
            "tesla.com",
            Some("Tesla"),
            None,
            &[],
            &[],
            popular(500, 20_000),
        )];
        let mut near = Vec::new();
        for i in 0..60 {
            let domain: &'static str = Box::leak(format!("ev{i}.example").into_boxed_str());
            records.push(site(
                domain,
                Some("Electric car maker"),
                None,
                &[],
                &[],
                obscure(5_000_000 + i, 2),
            ));
            near.push((domain, 1.0 - i as f32 / 1_000.0));
        }
        // Small sites that repeat the query's words are the 60 nearest;
        // the big site it describes comes after them, and is ranked.
        near.push(("tesla.com", 0.85));
        let (_dir, searcher) = build(&records);
        let hits = searcher
            .search_meaning(
                "electric car maker",
                100,
                &RankConfig::default(),
                &SearchOptions::default(),
                Some(&FixedMeaning(near)),
            )
            .unwrap()
            .hits;
        assert!(domains(&hits).contains(&"tesla.com"), "{hits:?}");
    }
}
