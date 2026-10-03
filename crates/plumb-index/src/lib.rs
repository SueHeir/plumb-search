//! The local search index: a Tantivy index over site records, ranked for
//! navigational queries ("us bank" should put usbank.com first).
//!
//! Ranking blends how well the query matches the site's names (BM25 over the
//! domain label, aliases, title, link text and description, plus a "joined"
//! match so "us bank" finds the label `usbank`) with the site's popularity
//! prior, [`plumb_core::link_score`]:
//! `score = alpha * link_score + (1 - alpha) * text_score`, where the text
//! score is normalized to `0..=1` within the candidates of each query.
//!
//! On top of the blend, a site whose domain label is the query
//! ([`RankConfig::exact_label_bonus`]), or one of whose aliases is
//! ([`RankConfig::exact_alias_bonus`]), gets a bonus. A query that is a
//! hostname or URL (`usbank.com`, `https://www.usbank.com/`) counts as an
//! exact label match for that domain.
//!
//! All text, at index and at query time, goes through
//! [`plumb_core::normalize_text`] and is then ASCII-folded, so `U.S. Bank`,
//! `us bank` and `US BANK` are the same query and `nestle` finds `Nestlé`.

mod analysis;
mod replace;
mod schema;

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::{registrable_domain, truncate_chars, SiteRecord, MAX_TEXT_CHARS};
use serde::{Deserialize, Serialize};
use tantivy::collector::{DocSetCollector, TopDocs};
use tantivy::merge_policy::NoMergePolicy;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{DocAddress, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

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
    /// How many BM25 candidates are re-ranked per query.
    pub candidates: usize,
    /// Added when the query, joined, equals the domain label (`us bank` -> `usbank`).
    pub exact_label_bonus: f32,
    /// Added instead when the query, joined, equals one of the site's
    /// aliases but not its label (`ally bank` for ally.com). Smaller than the
    /// label bonus because sites can pick their own aliases (`og:site_name`).
    pub exact_alias_bonus: f32,
}

impl Default for RankConfig {
    fn default() -> Self {
        RankConfig {
            alpha: 0.35,
            candidates: 200,
            exact_label_bonus: 0.25,
            exact_alias_bonus: 0.1,
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
    /// Normalized text match in `0..=1` (before the exact-label or alias bonus).
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
    /// The top `max(cfg.candidates, limit)` documents by BM25 are re-ranked
    /// by the blended score; ties go to the higher link score, then to the
    /// alphabetically first domain.
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

        let num_candidates = cfg.candidates.max(limit).min(num_docs);
        let candidates = searcher.search(
            &query.text_query(&self.fields),
            &TopDocs::with_limit(num_candidates).order_by_score(),
        )?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let mut label_terms = Vec::new();
        let mut alias_terms = Vec::new();
        if let Some(joined) = &query.joined {
            label_terms.push(Term::from_field_text(self.fields.label_key, joined));
            alias_terms.push(Term::from_field_text(self.fields.alias_key, joined));
        }
        if let Some(domain) = &query.domain {
            label_terms.push(Term::from_field_text(self.fields.domain, domain));
        }
        let label_matches = matching_docs(&searcher, label_terms)?;
        let alias_matches = matching_docs(&searcher, alias_terms)?;

        let columns = searcher
            .segment_readers()
            .iter()
            .map(|segment| {
                let fast = segment.fast_fields();
                Ok((fast.f64(schema::LINK_SCORE)?, fast.str(schema::DOMAIN)?))
            })
            .collect::<tantivy::Result<Vec<_>>>()?;

        let alpha = if cfg.alpha.is_finite() {
            cfg.alpha.clamp(0.0, 1.0)
        } else {
            RankConfig::default().alpha
        };
        let max_bm25 = candidates.iter().map(|&(bm25, _)| bm25).fold(0.0, f32::max);
        let mut ranked: Vec<Ranked> = candidates
            .into_iter()
            .map(|(bm25, addr)| {
                let (link_scores, domains) = &columns[addr.segment_ord as usize];
                let link_score = link_scores.first(addr.doc_id).unwrap_or(0.0) as f32;
                let text_score = if max_bm25 > 0.0 {
                    (bm25 / max_bm25).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let mut bonus: f32 = 0.0;
                if label_matches.contains(&addr) {
                    bonus = bonus.max(cfg.exact_label_bonus);
                }
                if alias_matches.contains(&addr) {
                    bonus = bonus.max(cfg.exact_alias_bonus);
                }
                // Within a segment, term ordinals sort like the domains themselves.
                let domain_ord = domains
                    .as_ref()
                    .and_then(|column| column.term_ords(addr.doc_id).next())
                    .unwrap_or(u64::MAX);
                Ranked {
                    addr,
                    score: alpha * link_score + (1.0 - alpha) * text_score + bonus,
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
    /// The registrable domain, when the query is a hostname or URL.
    domain: Option<String>,
}

impl ParsedQuery {
    /// `None` when the query has no letters or digits.
    fn new(query: &str, words: &TextAnalyzer, joined: &TextAnalyzer) -> Option<ParsedQuery> {
        let query = truncate_chars(query, MAX_TEXT_CHARS);
        let mut distinct = Vec::new();
        for word in analysis::tokens(words, &query) {
            if !distinct.contains(&word) {
                distinct.push(word);
            }
        }
        if distinct.is_empty() {
            return None;
        }
        distinct.truncate(MAX_QUERY_WORDS);
        Some(ParsedQuery {
            words: distinct,
            joined: analysis::tokens(joined, &query).into_iter().next(),
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
        let hits = searcher.search("us bank", 10).unwrap();
        assert!(hits.iter().any(|hit| hit.text_score == 1.0));
        for pair in hits.windows(2) {
            assert!(pair[0].score >= pair[1].score);
        }
        for hit in &hits {
            assert!((0.0..=1.0).contains(&hit.text_score), "{hit:?}");
            assert!((0.0..=1.0).contains(&hit.link_score), "{hit:?}");
            let bonus = if hit.domain == "usbank.com" {
                cfg.exact_label_bonus
            } else {
                0.0
            };
            let expected = cfg.alpha * hit.link_score + (1.0 - cfg.alpha) * hit.text_score + bonus;
            assert!((hit.score - expected).abs() < 1e-5, "{hit:?}");
        }
        let usbank = &hits[0];
        let record_score = corpus()[0].link_score();
        assert!((usbank.link_score - record_score).abs() < 1e-6);
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
                domain: None,
            }
        );
        let parsed = parse("bank bank BANK").unwrap();
        assert_eq!(parsed.words, ["bank"]);
        assert_eq!(parsed.joined.as_deref(), Some("bankbankbank"));
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
        assert_eq!(parse(&many).unwrap().words.len(), MAX_QUERY_WORDS);
    }

    #[test]
    fn partial_configs_deserialize_with_defaults() {
        let cfg: RankConfig = serde_json_like("alpha", 0.5);
        assert_eq!(cfg.alpha, 0.5);
        assert_eq!(cfg.candidates, RankConfig::default().candidates);
        assert_eq!(
            cfg.exact_alias_bonus,
            RankConfig::default().exact_alias_bonus
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
