//! The local search index: a Tantivy index over site records, ranked for
//! navigational queries ("us bank" should put usbank.com first).
//!
//! Ranking blends how well the query matches the site's names (BM25 over the
//! domain label, aliases, title, link text and description, plus a "joined"
//! match so "us bank" finds the label `usbank`) with the site's popularity
//! prior, [`plumb_core::link_score`]:
//!
//! `score = alpha * link_score + trust * ((1 - alpha) * text_score + name_bonus)`
//!
//! - `text_score` is the BM25 score normalized to `0..=1` within the
//!   candidates of each query.
//! - `name_bonus` rewards a site whose name the query starts with. A domain
//!   label equal to the first `k` of the query's `n` words gets `k / n` of
//!   [`RankConfig::exact_label_bonus`] (all of it when it is the whole
//!   query, `us bank` -> usbank.com); an alias likewise gets `k / n` of
//!   [`RankConfig::exact_alias_bonus`]. So in "irs refund" irs.gov gets half
//!   the label bonus. A query that is a hostname or URL (`usbank.com`,
//!   `https://www.usbank.com/`) counts as a whole-query label match for
//!   that domain.
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
//!
//! All text, at index and at query time, goes through
//! [`plumb_core::normalize_text`] and is then ASCII-folded, so `U.S. Bank`,
//! `us bank` and `US BANK` are the same query and `nestle` finds `Nestlé`.

mod analysis;
mod replace;
mod schema;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::{registrable_domain, truncate_chars, SiteRecord, MAX_TEXT_CHARS};
use serde::{Deserialize, Serialize};
use tantivy::collector::{DocSetCollector, TopDocs};
use tantivy::merge_policy::NoMergePolicy;
use tantivy::query::{BooleanQuery, BoostQuery, EnableScoring, Occur, Query, Scorer, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{
    DocAddress, DocSet, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term,
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
/// BM25 boost of the whole query, joined (`us bank` -> `usbank`), matching a
/// joined name or a label word.
const WHOLE_QUERY_BOOST: f32 = 6.0;
/// BM25 boost of a query that is the hostname or URL of an indexed domain.
const DOMAIN_BOOST: f32 = 10.0;
/// Most distinct query words used; the rest are ignored.
const MAX_QUERY_WORDS: usize = 16;
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
        }
    }
}

/// What [`build_index`] built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStats {
    /// Documents in the index: one per record with a non-empty domain.
    pub docs: u64,
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
}

/// Builds a fresh index in `dir`, replacing any index already there. Records
/// with the same domain must already be merged (one document per domain).
///
/// The index is built in a hidden directory next to `dir` and swapped in
/// only once it is complete, so a failed build leaves the previous index in
/// place and removes its own leftovers. A `Searcher` opened before keeps
/// serving the previous index (on Unix; elsewhere the swap may fail while
/// it is open).
///
/// Domains are trimmed and lowercased; records with an empty domain are
/// skipped, and a domain that appears twice is an error. To protect other
/// data, a non-empty `dir` that does not hold an index is refused. If `dir`
/// is a symlink, the directory it points to is replaced. Its parent must be
/// writable, and `dir` itself cannot be a mount point (mount the parent).
pub fn build_index(dir: &Path, records: &[SiteRecord]) -> Result<IndexStats> {
    let domains = clean_domains(records)?;
    let staging = Staging::new(dir)?;
    let docs = write_index(staging.path(), records, &domains)?;
    staging.install()?;
    Ok(IndexStats { docs })
}

/// Writes a complete index of `records` into the empty directory `dir`:
/// one commit, then a merge into a single segment.
fn write_index(dir: &Path, records: &[SiteRecord], domains: &[Option<String>]) -> Result<u64> {
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
    let mut docs = 0;
    for (record, domain) in records.iter().zip(domains) {
        let Some(domain) = domain else { continue };
        writer.add_document(schema::document(&fields, record, domain))?;
        docs += 1;
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
    Ok(docs)
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

/// The cleaned-up domain of each record, `None` for an empty one.
fn clean_domains(records: &[SiteRecord]) -> Result<Vec<Option<String>>> {
    let mut seen = HashSet::with_capacity(records.len());
    records
        .iter()
        .map(|record| {
            let domain = record
                .domain
                .trim()
                .trim_end_matches('.')
                .to_ascii_lowercase();
            if domain.is_empty() {
                return Ok(None);
            }
            if !seen.insert(domain.clone()) {
                bail!(
                    "domain {domain} appears in more than one record; \
                     merge records per domain (plumb_core::RecordSet) before indexing"
                );
            }
            Ok(Some(domain))
        })
        .collect()
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

    /// [`Searcher::search_with`] using [`RankConfig::default`].
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.search_with(query, limit, &RankConfig::default())
    }

    /// Best `limit` hits for `query`, best first. A query with no letters or
    /// digits returns no hits.
    ///
    /// The top `max(cfg.candidates, limit)` documents by BM25, plus every
    /// site whose name the query starts with, are re-ranked by the blended
    /// score (see the [crate docs](crate)); ties go to the higher link
    /// score, then to the alphabetically first domain.
    pub fn search_with(&self, query: &str, limit: usize, cfg: &RankConfig) -> Result<Vec<Hit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let Some(query) = ParsedQuery::new(query, &self.words, &self.joined) else {
            return Ok(Vec::new());
        };
        let searcher = self.reader.searcher();
        let num_docs = usize::try_from(searcher.num_docs()).unwrap_or(usize::MAX);
        if num_docs == 0 {
            return Ok(Vec::new());
        }

        let text_query = query.text_query(&self.fields);
        let num_candidates = cfg.candidates.max(limit).min(num_docs);
        let mut candidates = searcher.search(
            &text_query,
            &TopDocs::with_limit(num_candidates).order_by_score(),
        )?;
        // A site the query names is ranked even if BM25 put others first:
        // it is what the look-alikes are measured against.
        let names = self.name_matches(&searcher, &query)?;
        let known: HashSet<DocAddress> = candidates.iter().map(|&(_, addr)| addr).collect();
        let unranked = names.keys().filter(|addr| !known.contains(addr)).copied();
        candidates.extend(bm25_of(&searcher, &text_query, unranked.collect())?);
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let columns = searcher
            .segment_readers()
            .iter()
            .map(|segment| {
                let fast = segment.fast_fields();
                Ok((fast.f64(schema::LINK_SCORE)?, fast.str(schema::DOMAIN)?))
            })
            .collect::<tantivy::Result<Vec<_>>>()?;
        let link_score_of = |addr: DocAddress| {
            let (link_scores, _) = &columns[addr.segment_ord as usize];
            link_scores.first(addr.doc_id).unwrap_or(0.0) as f32
        };

        let default = RankConfig::default();
        let alpha = unit_or(cfg.alpha, default.alpha);
        let untrusted_share = unit_or(cfg.untrusted_share, default.untrusted_share);
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
        let query_words = query.len as f32;

        let max_bm25 = candidates.iter().map(|&(bm25, _)| bm25).fold(0.0, f32::max);
        let mut ranked: Vec<Ranked> = candidates
            .into_iter()
            .map(|(bm25, addr)| {
                let link_score = link_score_of(addr);
                let text_score = if max_bm25 > 0.0 {
                    (bm25 / max_bm25).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let name = names.get(&addr).copied().unwrap_or_default();
                let name_bonus = (cfg.exact_label_bonus * name.label as f32 / query_words)
                    .max(cfg.exact_alias_bonus * name.alias as f32 / query_words);
                let trust = if name.typed || trusted_link_score <= 0.0 {
                    1.0
                } else {
                    let evidence = (link_score / trusted_link_score).min(1.0);
                    untrusted_share + (1.0 - untrusted_share) * evidence
                };
                // Within a segment, term ordinals sort like the domains themselves.
                let (_, domains) = &columns[addr.segment_ord as usize];
                let domain_ord = domains
                    .as_ref()
                    .and_then(|column| column.term_ords(addr.doc_id).next())
                    .unwrap_or(u64::MAX);
                Ranked {
                    addr,
                    score: alpha * link_score + trust * ((1.0 - alpha) * text_score + name_bonus),
                    text_score,
                    link_score,
                    tie_break: (addr.segment_ord, domain_ord),
                }
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| b.link_score.total_cmp(&a.link_score))
                .then_with(|| a.tie_break.cmp(&b.tie_break))
        });
        ranked.truncate(limit);

        ranked
            .into_iter()
            .map(|ranked| self.hit(&searcher, &ranked))
            .collect()
    }

    /// The sites whose domain label or an alias equals the query's first
    /// words, with how many words each covers, plus the site of a typed
    /// hostname (covering the whole query).
    fn name_matches(
        &self,
        searcher: &tantivy::Searcher,
        query: &ParsedQuery,
    ) -> Result<HashMap<DocAddress, NameMatch>> {
        let mut names: HashMap<DocAddress, NameMatch> = HashMap::new();
        for (i, key) in query.leading.iter().enumerate() {
            let words = i + 1;
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

    /// Reads the stored fields of a ranked document.
    fn hit(&self, searcher: &tantivy::Searcher, ranked: &Ranked) -> Result<Hit> {
        let doc: TantivyDocument = searcher.doc(ranked.addr)?;
        let text = |field| {
            doc.get_first(field)
                .and_then(|value| value.as_str())
                .map(str::to_string)
        };
        let domain = text(self.fields.domain).unwrap_or_default();
        let url = text(self.fields.url).unwrap_or_else(|| format!("https://{domain}/"));
        Ok(Hit {
            url,
            title: text(self.fields.title),
            description: text(self.fields.description),
            domain,
            score: ranked.score,
            text_score: ranked.text_score,
            link_score: ranked.link_score,
        })
    }
}

/// A candidate with its blended score.
struct Ranked {
    addr: DocAddress,
    score: f32,
    text_score: f32,
    link_score: f32,
    tie_break: (u32, u64),
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
    /// The whole query as one joined token: `U.S. Bank` -> `usbank`.
    joined: Option<String>,
    /// The first word, the first two joined, and so on (at most
    /// [`MAX_QUERY_WORDS`]): `us bank login` -> `us`, `usbank`,
    /// `usbanklogin`. The names a site can have to be named by the query.
    leading: Vec<String>,
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
        let leading = tokens
            .iter()
            .take(MAX_QUERY_WORDS)
            .scan(String::new(), |key, word| {
                key.push_str(word);
                Some(key.clone())
            })
            .collect();
        Some(ParsedQuery {
            words: distinct,
            joined: analysis::tokens(joined, &query).into_iter().next(),
            leading,
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
    fn text_query(&self, f: &Fields) -> BooleanQuery {
        let mut clauses = Clauses::default();
        let name_share = 1.0 / self.words.len() as f32;
        let per_word = [
            (f.label, LABEL_BOOST * name_share),
            (f.joined, JOINED_BOOST * name_share),
            (f.aliases, ALIASES_BOOST),
            (f.title, TITLE_BOOST),
            (f.anchors, ANCHORS_BOOST),
            (f.description, DESCRIPTION_BOOST),
        ];
        for word in &self.words {
            for (field, boost) in per_word {
                clauses.add(Term::from_field_text(field, word), boost);
            }
        }
        if let Some(joined) = &self.joined {
            clauses.add(Term::from_field_text(f.joined, joined), WHOLE_QUERY_BOOST);
            clauses.add(Term::from_field_text(f.label, joined), WHOLE_QUERY_BOOST);
        }
        if let Some(domain) = &self.domain {
            clauses.add(Term::from_field_text(f.domain, domain), DOMAIN_BOOST);
        }
        clauses.into_query()
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
        for (text, count) in link_texts {
            record.add_link_text(text, *count);
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

        // No site is named "online" or "online banking": a plain blend.
        let hits = searcher.search("online banking", 10).unwrap();
        check(&hits, &|hit| {
            cfg.alpha * hit.link_score + (1.0 - cfg.alpha) * hit.text_score
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

        // "us bank login" goes on past the name: usbank.com gets 2/3 of the
        // bonus, and sites below the trusted link score lose some of theirs.
        let hits = searcher.search("us bank login", 10).unwrap();
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

        // Without the trust rule the stuffed titles win, which is what it is for.
        let untrusting = RankConfig {
            trusted_link_score: 0.0,
            ..RankConfig::default()
        };
        let hits = searcher
            .search_with("us bank login", 1, &untrusting)
            .unwrap();
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

        // Rejected before anything is written.
        let twice = [SiteRecord::new("a.com"), SiteRecord::new("A.com ")];
        let err = build_index(&dir, &twice).unwrap_err();
        assert!(err.to_string().contains("a.com"), "{err:#}");
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
        assert_eq!(build_index(&missing, &[]).unwrap().docs, 0);
        assert_eq!(Searcher::open(&missing).unwrap().num_docs(), 0);
        let empty = root.path().join("empty");
        fs::create_dir(&empty).unwrap();
        assert_eq!(build_index(&empty, &corpus()[..2]).unwrap().docs, 2);
        assert_eq!(entries(root.path()), ["a", "data", "empty"]);
    }

    #[test]
    fn records_without_a_domain_are_skipped() {
        let dir = TempDir::new().unwrap();
        let records = [SiteRecord::new("  "), SiteRecord::new("Example.COM")];
        assert_eq!(build_index(dir.path(), &records).unwrap().docs, 1);
        let searcher = Searcher::open(dir.path()).unwrap();
        assert_eq!(top(&searcher, "example"), "example.com");
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
            .map(|i| LinkText {
                text: format!("anchor {i}"),
                count: i,
            })
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
                joined: Some("usbank".into()),
                leading: vec!["us".into(), "usbank".into()],
                len: 2,
                domain: None,
            }
        );
        let parsed = parse("bank bank BANK").unwrap();
        assert_eq!(parsed.words, ["bank"]);
        assert_eq!(parsed.joined.as_deref(), Some("bankbankbank"));
        assert_eq!(parsed.leading, ["bank", "bankbank", "bankbankbank"]);
        assert_eq!(parsed.len, 3);
        // Names are compared as indexed: folded, lowercased, punctuation gone.
        assert_eq!(
            parse("Nestlé S.A. login").unwrap().leading,
            ["nestle", "nestlesa", "nestlesalogin"]
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
                    record.add_link_text(&text, 1 + rng.below(200) as u32);
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
}
