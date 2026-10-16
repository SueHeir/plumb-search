//! The local search index: a Tantivy index over sites, ranked for
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
//! - A text most of the sites linking with it link to one site with names
//!   that site like an alias, and in full like its label when it is the
//!   whole query ([`RankConfig::link_names`]): "the guardian" names
//!   theguardian.com because most links saying "the guardian" go there,
//!   whatever its own pages call it.
//! - A query that is a kind of thing ("banks", "airlines") gives every site
//!   of that kind ([`SiteRecord::kinds`](plumb_core::SiteRecord::kinds)) a
//!   full text match and [`RankConfig::kind_bonus`], so they are listed by
//!   popularity.
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
//! A query that names no site in full is checked for typos. It is
//! searched as typed and a correction is only suggested: "amazom" asks "Did
//! you mean amazon?". Only when no site has all the words typed and the
//! correction finds one that does are its results shown instead ("kayne
//! west albums"; [`Spelling::applied`]). See [`Searcher::search_meaning`]
//! and the [`spell`] module.
//!
//! All text, at index and at query time, goes through
//! [`plumb_core::normalize_text`] and is then ASCII-folded, so `U.S. Bank`,
//! `us bank` and `US BANK` are the same query and `nestle` finds `Nestlé`.

mod analysis;
pub mod candidates;
pub mod health;
mod hits;
mod indexing;
pub mod items;
mod junk;
mod language;
pub mod learned;
mod names;
pub mod pages;
pub mod places;
mod query;
mod query_evidence;
mod rank;
mod replace;
mod schema;
mod search;
mod spell;
pub mod spell_model;
mod states;
mod storage;
mod topics;

use std::collections::HashSet;

use plumb_core::{language_code, normalize_country, normalize_text, KeyPage, SafeSearch};
use serde::{Deserialize, Serialize};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::IndexReader;

pub use crate::analysis::same_words;
pub use crate::hits::suggested_site_second;
pub use crate::indexing::{
    build_index, build_index_with_budget, redirect_name, redirect_targets, IndexBuild,
};
pub use crate::names::without_intent_words;
pub use crate::query_evidence::CandidateEvidence;
pub use crate::replace::remove_build_leftovers;
use crate::schema::Fields;
pub use crate::states::{asked_state_first, namesakes_last};

// Function words ("pizza in denver", "bank of america") name no site in a
// longer query: between two other words they never match a domain label or
// joined name on their own, so in.gov is not found by them, and one alone
// never counts as a leading name ("in n out burger" does not name in.gov). A
// one-word query is still a name.
use plumb_core::is_function_word;

/// Words that ask or say what is wanted rather than name anything: in a
/// query of two words or more, one of them never matches a domain label,
/// joined name or other name on its own, nor counts as a leading name, so
/// who.int is not found for "who wrote hamlet", in.gov for "ups sign in",
/// website.ws for "amtrak website" or bestbuy.com for "best mattress". The
/// whole query joined still names a site ("best buy", "my chart").
const FILLER_WORDS: &[&str] = &[
    // Asking.
    "how", "why", "what", "whats", "when", "where", "which", "who", "whom", "whose", "can", "could",
    "should", "do", "does", "did", "is", "are", "was", "were", "will", "would",
    // Wanting.
    "my", "me", "your", "best", "top", "open", "now", "today", "hours", "near", "nearby", "website",
    "site", "homepage", "app", "login", "signin", "sign", "log", "account", "contact", "support",
    "help", "official",
];

/// Most distinct query words used; the rest are ignored.
const MAX_QUERY_WORDS: usize = 16;

/// A word found in this many sites or more is spelled right as far as the
/// sites can tell ([`Searcher::word_sites`]).
pub const KNOWN_WORD_SITES: u64 = spell::KNOWN_WORD_DOCS;

/// The least link score of a well-known site (roughly the top 30,000).
pub const WELL_KNOWN_LINK_SCORE: f32 = 0.5;

/// The share of the sites linking with a text that must link to one site
/// for the text to name it ([`RankConfig::link_names`]): the probability of
/// the site given the text, as CrossWikis estimates it from Wikipedia's
/// links. Over half, so a text names one site at most, and generic ones
/// ("click here", "official website") name none.
pub const LINK_NAME_SHARE: f32 = 0.5;

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
    /// The same floor for queries that name what they look for ("twin
    /// peaks", "us bank"): a site the query does not name gets its
    /// popularity in full only once its text matches this well, so
    /// wix.com or youtube.com, matching nothing of it, are not listed for
    /// being big. `None` turns it off.
    pub navigational_relevance: Option<f32>,
    /// The same floor for queries asked as a question ("what is the
    /// smallest country", "how to boil an egg"): the answer is a page, and
    /// a site matching next to none of the words is not one, however
    /// popular (cloudflare.com and stripe.com led such searches at a text
    /// match of 0.03). The higher of this and the other floor applies.
    /// `None` turns it off.
    pub question_relevance: Option<f32>,
    /// For a site that has none of the query's words and is found only
    /// for being near it in meaning, the text match (meaning alone) it
    /// needs for its popularity to count in full; below it, popularity
    /// counts in proportion. The nearest sites in meaning include big
    /// ones the query has nothing to do with ("how to unclog a drain"
    /// found stripe.com and snapchat.com, at a text match of 0.03 to
    /// 0.06), which popularity alone would put on the first page. `None`
    /// turns it off.
    pub meaning_only_relevance: Option<f32>,
    /// When the best site is one the query names, sites scoring less than
    /// this share of its score are left out unless the query names them
    /// too: what ranks that far below the site searched for is filler
    /// ("bank of america login" listed apple.com, google.com and
    /// cloudflare.com, matching none of it). `None` keeps them.
    pub named_share: Option<f32>,
    /// When the query of two words or more is the whole name of a site
    /// with a Wikidata item, other names or many links, other sites are
    /// left out unless they have every word of it, or the query names them
    /// too, or they are of the kind it names: having one word is not having
    /// the name ("twin peaks" listed twinfallscounty.org and peakdesign.com,
    /// "us bank" worldbank.org and us.es). A namesake of plain words
    /// (vacationrentals.com) leaves the rest listed: airbnb.com is still
    /// what "vacation rentals" is after.
    pub named_needs_all_words: bool,
    /// With a [`Meaning`], for a query of two main words or more that no
    /// site is named by in full: the closeness in meaning a site lacking
    /// one of its main words needs to be listed at all. Matching a word or
    /// two is no answer: "secret of mana walkthrough" listed
    /// victoriassecret.com, "numbness on top of foot" flashscore.fr
    /// ("Foot en direct"), and nothing near in meaning stands for them. A
    /// site the query names by its first words is kept, unless the words
    /// go on from the name as one phrase ("the cricket and the ant" is not
    /// cricket.com.au's). Fewer results are shown rather than these.
    /// `None` keeps them.
    pub partial_closeness: Option<f32>,
    /// Opt-in experiment: measure substantive coverage before giving
    /// partial names or popularity their boost. Exact names, typed domains
    /// and the existing explicit navigation intents retain their controls.
    /// Disabled by default; diagnostic query evidence, source quality and
    /// intent parsing remain independent of coverage/tier adjustments.
    pub whole_query_relevance: bool,
    /// BM25 boost of a query word matching a site's search terms
    /// ([`plumb_core::SiteRecord::terms`]), picked from its whole homepage.
    pub terms_boost: f32,
    /// Words that ask or say what is wanted ([`FILLER_WORDS`]) name no site
    /// in a query of two words or more.
    pub filler_words: bool,
    /// A query of three words or more asked as a question ("how to get
    /// rid of aphids") names no site by its words: none of them matches a
    /// domain label or joined name on its own, and its first words are no
    /// leading name. Questions describe what they want; rid.it and get.gov
    /// are not it. Only a text most links to a site use still names it
    /// inside a question ([`RankConfig::link_names`]), so it is kept where
    /// it ranks: netflix.com in "how to cancel netflix".
    pub questions_name_nothing: bool,
    /// Add the official site of the article the query names when no site
    /// found is it ([`pages::add_named_site`]).
    pub add_named_site: bool,
    /// Leave out sites lacking some of the query's main words when the
    /// whole query names an article ([`pages::drop_namesakes_of_words`]).
    /// Off: the test searches came out better without it (17 better, 14
    /// worse), the described ones most of all.
    pub drop_namesakes: bool,
    /// Put the first results in the order the learned ranking gives
    /// ([`learned::reorder_results`]), once pages are placed among the
    /// sites.
    pub learned: bool,
    /// With [`RankConfig::learned`] and a [`Meaning`], put the first
    /// [`learned::LEARNED_SITES`] sites in the order the site model gives
    /// ([`learned::reorder_sites`]) before anything else moves them. The
    /// learned order then only places pages among them.
    pub learned_sites: bool,
    /// Rank typo corrections by the spelling model learned from the
    /// index's words ([`spell_model`]): what the edits cost against how
    /// common the correction is. Without it, or without a model, fewest
    /// edits win, then popularity.
    pub spelling_channel: bool,
    /// Correct a word the index knows when the spelling model takes it for
    /// a slip of a far commoner word ("amtrack"), weighing how much
    /// commoner by this power. `None` never corrects a known word alone.
    pub real_word_weight: Option<f32>,
    /// Correct a word of a longer query when the word-pair model makes
    /// another word far likelier there ("capitol one"), weighing the
    /// word-pair scores by this power. `None` leaves such words.
    pub context_weight: Option<f32>,
    /// A site most links with a text point to is named by that text, as by
    /// an alias: "the guardian" names theguardian.com when most of the
    /// sites linking with those words link there ([`LINK_NAME_SHARE`]).
    /// Needs an index built with link names.
    pub link_names: bool,
    /// Added instead of the alias bonus when a link text names the site
    /// ([`RankConfig::link_names`]), and `k / n` of it for one equal to the
    /// first `k` words. More than the label bonus: when most of the sites
    /// linking with "steam" link to steampowered.com, that names it more
    /// surely than steam.tv's domain names steam.tv. 0.4 did best of 0.1,
    /// 0.25 and 0.4 on the test searches (brand 93.4% to 94.9% first on
    /// the tune half, 93.7% to 94.7% on the held-out one).
    pub link_name_bonus: f32,
    /// Medical searches ("ibuprofen dosage", "flu symptoms") list a few
    /// health authorities first ([`health::authorities_first`]).
    pub health_authorities: bool,
    /// For a query asking only for the news ("news", "world news"), rank
    /// the well-known sites that say they are news as sites of the kind
    /// it names, and let the news words name no site
    /// ([`topics::asks_only_for_news`]): nytimes.com and reuters.com, not
    /// news.cn and news.by.
    pub news_sites: bool,
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
            navigational_relevance: Some(0.05),
            question_relevance: Some(0.3),
            meaning_only_relevance: Some(0.35),
            named_share: Some(0.4),
            named_needs_all_words: true,
            partial_closeness: Some(0.5),
            whole_query_relevance: false,
            terms_boost: 1.0,
            filler_words: true,
            questions_name_nothing: true,
            add_named_site: true,
            drop_namesakes: false,
            learned: true,
            learned_sites: true,
            spelling_channel: true,
            real_word_weight: None,
            context_weight: None,
            link_names: true,
            link_name_bonus: 0.4,
            health_authorities: true,
            news_sites: true,
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
    /// [`Meaning::closeness`] as the query alone gives it, when that
    /// differs (a query embedded after an instruction to the model); it
    /// decides only whether a page goes before the sites
    /// ([`Hit::placing_text_score`]).
    fn plain_closeness(&self, _domain: &str) -> Option<Option<f32>> {
        None
    }
}

/// What [`build_index`] built. Every site given is a document, merged into
/// another one, skipped or folded into the site it redirects to: `docs +
/// merged + skipped + redirected` is the number of sites given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStats {
    /// Documents in the index, one per domain.
    pub docs: u64,
    /// Sites merged into an earlier one of the same domain.
    #[serde(default)]
    pub merged: u64,
    /// Sites skipped because their domain is not a valid registrable
    /// domain ([`plumb_core::canonical_domain`] rejects it), empty ones
    /// included, or because the site is gone
    /// ([`plumb_core::site::crawl_marks`]).
    #[serde(default)]
    pub skipped: u64,
    /// Sites left out because their homepage redirects to another site in
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
    /// [`Hit::text_score`] by the query's plain meaning, when the ranking
    /// used another ([`Meaning::plain_closeness`]), for
    /// [`pages::place_pages`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placing_text_score: Option<f32>,
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
    /// How much the Wikipedia article about what the site is gets read, as
    /// [`pages::PageHit::popularity`], when a page index says
    /// ([`pages::PageSearcher::note_demand`]); see [`pages::place_pages`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demand: Option<f32>,
    /// The query, of two words or more, does not name the site and the
    /// site lacks some of its main words (all but [`FILLER_WORDS`] and
    /// [`plumb_core::FUNCTION_WORDS`]): better.com for "better call saul".
    /// Such a site is filler once a page is named by the whole query
    /// ([`pages::drop_namesakes_of_words`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub missing_words: bool,
    /// Query evidence unaffected by learned/display score trading. Older
    /// peers and synthetic/source-routed hits may have no such evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_evidence: Option<CandidateEvidence>,
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
    /// one (a language code, `en`), as it declares or as its script or
    /// common words tell ([`plumb_core::site_language`]). Sites none of
    /// these tells stay, and so do sites the whole query names.
    pub language: Option<String>,
    /// How the results page shows recent headlines; the index ignores it.
    pub recent: plumb_core::RecentNews,
}

impl SearchOptions {
    /// [`SearchOptions::language`] for `query`: none when the query is
    /// written partly in a script that language is not, "天気予報" with
    /// English chosen, or the filter would leave out the very sites the
    /// query is written for ([`plumb_core::written_outside`]).
    pub fn language_for(&self, query: &str) -> Option<&str> {
        self.language
            .as_deref()
            .filter(|wanted| !plumb_core::written_outside(wanted, query))
    }
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

/// Where the sites a query ranks come from ([`Searcher::candidate_pool`]).
/// A site can come from more than one place.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CandidatePool {
    /// The first [`RankConfig::candidates`] sites by BM25, best first.
    pub words: Vec<String>,
    /// The most linked sites with any of the query's words.
    pub popular: Vec<String>,
    /// The sites the query names, by its first words or all of them.
    pub named: Vec<String>,
    /// The sites of the kind the query names ("banks").
    pub kind: Vec<String>,
    /// The sites near the query in meaning that are ranked.
    pub meaning: Vec<String>,
    /// Independent relevance and quality diagnostics for eligible
    /// candidates, before pruning or downstream order-score changes.
    pub evidence: Vec<CandidateEvidence>,
}

impl CandidatePool {
    /// Whether the site `domain` is ranked at all.
    pub fn has(&self, domain: &str) -> bool {
        [
            &self.words,
            &self.popular,
            &self.named,
            &self.kind,
            &self.meaning,
        ]
        .iter()
        .any(|list| list.iter().any(|d| d == domain))
    }
}

/// The sites by how well their words match a query
/// ([`Searcher::words_order`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WordsOrder {
    /// The first sites, best first.
    pub order: Vec<String>,
    /// Those of the domains asked about that have any of the query's words.
    pub matched: HashSet<String>,
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
    /// The well-known site the corrected spelling names, when the typo was
    /// in a site's name ("youtbue": youtube.com), offered with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// The results are for this spelling, not the query as typed: nothing
    /// found had all the typed words, and the spelling finds a site that
    /// does ("kayne west stronger"). Shown as "Showing results for ...".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub applied: bool,
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
    /// What the index's words teach about spelling; `None` for an index
    /// built before it was kept ([`spell_model`]).
    spelling: Option<spell_model::Model>,
}

#[cfg(test)]
mod tests;
