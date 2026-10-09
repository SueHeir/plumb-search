//! `plumb eval`: the brand-name test. For each query in a TSV file, where
//! does the expected official site rank?
//!
//! The file has one `query<TAB>expected_domain[,another_ok_domain]` per
//! line; blank lines and lines starting with `#` are skipped. An expected
//! answer can also be a page's address (`https://en.wikipedia.org/wiki/
//! Marie_Curie`): with `--pages`, pages are listed among the sites as a
//! node lists them (see [`plumb_index::pages::place_pages`]), and a page
//! shown under a site's result counts at that site's rank. An address
//! ending in `*` takes any page whose address starts with the rest
//! (`https://diy.stackexchange.com/questions/*`). The metrics
//! are top-1 and top-3 rates and the mean reciprocal rank within the
//! results fetched ([`Metrics::from_ranks`]).

use std::fmt::Write as _;

use anyhow::{bail, Context, Result};
use plumb_core::registrable_domain;
use std::path::Path;

use plumb_index::pages::{
    add_named_site, drop_namesakes_of_words, lift_named_sites, place_pages, Page, PageSearcher,
    PlacedPage,
};
use plumb_index::{Hit, Meaning, SearchOptions, Searcher};
use tracing::info;

use crate::cli::{EvalArgs, Half};
use crate::meaning::MeaningIndex;

/// One query of a queries file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalQuery {
    /// 1-based line number in the file, for messages.
    pub line: usize,
    pub query: String,
    /// Registrable domains that count as the right answer.
    pub expected: Vec<String>,
}

/// Parses a queries file. Expected domains are reduced to registrable
/// domains (`www.usbank.com` -> `usbank.com`) to match how hits are keyed.
pub fn parse_queries(text: &str) -> Result<Vec<EvalQuery>> {
    // Editors such as Notepad start the file with a byte-order mark.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut queries = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((query, expected)) = raw.split_once('\t') else {
            bail!("line {line}: expected `query<TAB>domain`, found no tab in {trimmed:?}");
        };
        let query = query.trim();
        if query.is_empty() {
            bail!("line {line}: the query is empty");
        }
        let expected: Vec<String> = expected
            .split(',')
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(normalize_domain)
            .collect();
        if expected.is_empty() {
            bail!("line {line}: no expected domain for {query:?}");
        }
        queries.push(EvalQuery {
            line,
            query: query.to_string(),
            expected,
        });
    }
    Ok(queries)
}

/// The half of a queries file `query` is in. A query's half depends on its
/// words alone (lowercased, spaces collapsed), not on its line or file, so
/// adding queries never moves one from half to half, and a query asked in
/// two files is in the same half of both.
pub fn half_of(query: &str) -> Half {
    // FNV-1a and a final mix (MurmurHash3's), which never change between
    // Rust versions as std's hasher may.
    let words = query.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut hash = words
        .to_lowercase()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^= hash >> 33;
    if hash & 1 == 0 {
        Half::Tune
    } else {
        Half::HeldOut
    }
}

fn normalize_domain(domain: &str) -> String {
    // A page's address stays one; a homepage's counts as its site.
    if let Ok(url) = url::Url::parse(domain) {
        if matches!(url.scheme(), "http" | "https") && url.path() != "/" {
            return domain.to_string();
        }
    }
    registrable_domain(domain).unwrap_or_else(|| domain.trim_end_matches('.').to_ascii_lowercase())
}

/// Whether a result keyed `key` (a domain or a page's address) is the
/// expected answer `expected`, which takes any address it starts when it
/// ends in `*`.
pub fn is_expected(expected: &str, key: &str) -> bool {
    match expected.strip_suffix('*') {
        Some(start) => key.starts_with(start),
        None => expected == key,
    }
}

/// 1-based position of the first result whose domain is one of `expected`.
pub fn rank_of<S: AsRef<str>>(results: &[S], expected: &[String]) -> Option<usize> {
    results
        .iter()
        .position(|domain| expected.iter().any(|e| is_expected(e, domain.as_ref())))
        .map(|i| i + 1)
}

/// Totals of an evaluation run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub queries: usize,
    /// Queries whose expected site came first.
    pub top1: usize,
    /// Queries whose expected site was among the first three.
    pub top3: usize,
    /// Mean reciprocal rank within the results fetched (MRR@limit): a site
    /// at rank `r <= limit` scores `1/r`, a missing one scores 0.
    pub mrr: f64,
}

impl Metrics {
    /// Metrics from the rank each query's expected site was found at
    /// (`None` when it was not among the results). Ranks above `limit`
    /// count as not found.
    pub fn from_ranks(ranks: &[Option<usize>], limit: usize) -> Metrics {
        let found = |max: usize| {
            ranks
                .iter()
                .filter(|r| r.is_some_and(|r| r >= 1 && r <= max.min(limit)))
                .count()
        };
        let reciprocal_sum: f64 = ranks
            .iter()
            .flatten()
            .filter(|&&r| r >= 1 && r <= limit)
            .map(|&r| 1.0 / r as f64)
            .sum();
        Metrics {
            queries: ranks.len(),
            top1: found(1),
            top3: found(3),
            mrr: if ranks.is_empty() {
                0.0
            } else {
                reciprocal_sum / ranks.len() as f64
            },
        }
    }

    /// Share of queries answered at rank 1, from 0 to 1.
    pub fn top1_rate(&self) -> f64 {
        ratio(self.top1, self.queries)
    }

    /// Share of queries answered within the first three results, from 0 to 1.
    pub fn top3_rate(&self) -> f64 {
        ratio(self.top3, self.queries)
    }
}

fn ratio(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

/// One file of queries, read.
struct Suite {
    /// The file's name without its folder and extension (`brand_queries`).
    name: String,
    queries: Vec<EvalQuery>,
}

/// The queries of the file at `path`, only those of `half` when given.
fn read_suite(path: &Path, half: Option<Half>) -> Result<Suite> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let queries = parse_queries(&text).with_context(|| format!("parsing {}", path.display()))?;
    let queries: Vec<EvalQuery> = queries
        .into_iter()
        .filter(|q| half.is_none_or(|half| half_of(&q.query) == half))
        .collect();
    if queries.is_empty() {
        bail!("{} has no queries", path.display());
    }
    let name = path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("queries")
        .to_string();
    Ok(Suite { name, queries })
}

/// What every query is searched with.
struct Setup {
    searcher: Searcher,
    meaning: Option<MeaningIndex>,
    pages: Option<PageSearcher>,
    /// Holds the page index while it is searched.
    _pages_dir: tempfile::TempDir,
}

pub fn run(args: EvalArgs) -> Result<()> {
    let suites = args
        .queries
        .iter()
        .map(|path| read_suite(path, args.half))
        .collect::<Result<Vec<_>>>()?;
    let mut cfg = args.rank.unwrap_or_default();
    if let Some(alpha) = args.alpha {
        cfg.alpha = alpha;
    }
    let variants = match &args.sweep {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            parse_sweep(&text, &cfg).with_context(|| format!("parsing {}", path.display()))?
        }
        None => Vec::new(),
    };
    let setup = open_setup(&args)?;
    if variants.is_empty() {
        let mut low = None;
        let mut features_out = String::new();
        let rerankers = args
            .rerank_model
            .iter()
            .map(|dir| {
                plumb_embed::Reranker::load(dir)
                    .with_context(|| format!("loading the reranker in {}", dir.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        for suite in &suites {
            let mut features = args.features_out.as_ref().map(|_| Vec::new());
            if suites.len() > 1 {
                println!("== {}", suite.name);
            }
            info!(
                "evaluating {} queries against {} sites ({cfg:?})",
                suite.queries.len(),
                setup.searcher.num_docs(),
            );
            let ranks = evaluate(&args, &setup, &suite.queries, &cfg, true, features.as_mut())?;
            for mut query in features.into_iter().flatten() {
                if let serde_json::Value::Object(fields) = &mut query {
                    fields.insert("suite".into(), suite.name.clone().into());
                }
                rerank_rows(&rerankers, &mut query)?;
                let _ = writeln!(features_out, "{query}");
            }
            let metrics = Metrics::from_ranks(&ranks, args.limit);
            print!("{}", format_totals(&metrics, args.limit));
            if let Some(min) = args.min_top1 {
                if metrics.top1_rate() < min {
                    low = Some((suite.name.clone(), metrics.top1_rate()));
                }
            }
        }
        if let Some(path) = &args.features_out {
            std::fs::write(path, features_out)
                .with_context(|| format!("writing {}", path.display()))?;
        }
        if let (Some((name, rate)), Some(min)) = (low, args.min_top1) {
            bail!(
                "{name}: top-1 is {:.1}%, below --min-top1 {:.1}%",
                rate * 100.0,
                min * 100.0
            );
        }
        return Ok(());
    }
    sweep(&args, &setup, &suites, &variants)
}

fn open_setup(args: &EvalArgs) -> Result<Setup> {
    let searcher = Searcher::open(&args.index)
        .with_context(|| format!("opening the index in {}", args.index.display()))?;
    let meaning = MeaningIndex::from_args(&args.meaning)?;
    let pages_dir = tempfile::tempdir().context("making a folder for the page index")?;
    let pages = if args.pages.is_empty() {
        None
    } else {
        let mut all: Vec<Page> = Vec::new();
        for file in &args.pages {
            let reader = plumb_ingest::open_maybe_gz(file)?;
            let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let articles = plumb_core::article::read_articles(reader, args.pages_top)?;
            info!("indexing {} pages of {}", articles.len(), file.display());
            let set = set_of_file(name);
            all.extend(articles.into_iter().filter_map(|a| Page::from_set(&set, a)));
        }
        plumb_index::pages::build_page_index(pages_dir.path(), all)?;
        Some(PageSearcher::open(pages_dir.path())?)
    };
    Ok(Setup {
        searcher,
        meaning,
        pages,
        _pages_dir: pages_dir,
    })
}

/// The rank each query's expected answer was found at, within
/// `args.limit`. With `verbose`, misses are printed (and `--show`,
/// `--explain` say more).
fn evaluate(
    args: &EvalArgs,
    setup: &Setup,
    queries: &[EvalQuery],
    cfg: &plumb_index::RankConfig,
    verbose: bool,
    mut features: Option<&mut Vec<serde_json::Value>>,
) -> Result<Vec<Option<usize>>> {
    let Setup {
        searcher,
        meaning,
        pages,
        ..
    } = setup;
    // With --explain, sites ranked below the limit are fetched too, to show
    // how far behind the expected one is.
    let fetched = if args.explain && verbose {
        args.limit.max(EXPLAIN_DEPTH)
    } else {
        args.limit
    };

    let mut ranks = Vec::with_capacity(queries.len());
    for q in queries {
        if args.facts {
            ranks.push(fact_rank(args, q, searcher, cfg, pages.as_ref(), verbose)?);
            continue;
        }
        let options = SearchOptions {
            country: args.country.clone(),
            only_country: false,
            exact: args.exact,
            language: args.lang.clone(),
            ..SearchOptions::default()
        };
        let search = |query: &str| {
            let query_meaning = meaning.as_ref().and_then(|meaning| meaning.query(query));
            let results = searcher
                .search_meaning(
                    query,
                    fetched,
                    cfg,
                    &options,
                    query_meaning
                        .as_ref()
                        .map(|m| m as &dyn plumb_index::Meaning),
                )
                .with_context(|| format!("searching for {query:?}"));
            results.map(|results| (results, query_meaning))
        };
        let (mut results, mut query_meaning) = search(&q.query)?;
        // What one click on "Did you mean" finds.
        let mut searched = q.query.clone();
        if args.follow_suggestions {
            if let Some(spelling) = results.spelling.take() {
                searched = spelling.query;
                (results, query_meaning) = search(&searched)?;
            }
        }
        let hits = results.hits;
        let domains: Vec<&str> = hits.iter().map(|h| h.domain.as_str()).collect();
        // What came first: a page when one was listed first.
        let mut first = domains.first().map(|d| d.to_string());
        // With pages, what each listed position holds (pages count as rows).
        let mut listed = None;
        let deep_rank = match &pages {
            None => {
                if let Some(features) = features.as_deref_mut() {
                    let closeness = |domain: &str| {
                        query_meaning
                            .as_ref()
                            .and_then(|meaning| meaning.closeness(domain))
                    };
                    features.push(feature_rows(q, &searched, &hits, &[], &closeness));
                }
                rank_of(&domains, &q.expected)
            }
            Some(pages) => {
                let found = pages
                    .search(&searched, 10)
                    .with_context(|| format!("searching pages for {searched:?}"))?;
                let mut lifted = hits.clone();
                if cfg.add_named_site {
                    add_named_site(&mut lifted, &found, |domain| {
                        searcher.site(domain).ok().flatten()
                    });
                }
                if cfg.drop_namesakes {
                    drop_namesakes_of_words(&mut lifted, &found);
                }
                lift_named_sites(&mut lifted, &found);
                pages.note_demand(&mut lifted)?;
                let mut placed = place_pages(&searched, &lifted, found);
                // --features-out writes the hand-made order the learned
                // ranking is trained to improve.
                if cfg.learned && features.is_none() {
                    plumb_index::learned::reorder(
                        plumb_index::learned::Model::builtin(),
                        &searched,
                        &mut lifted,
                        &mut placed,
                    );
                }
                if let Some(features) = features.as_deref_mut() {
                    let closeness = |domain: &str| {
                        query_meaning
                            .as_ref()
                            .and_then(|meaning| meaning.closeness(domain))
                    };
                    features.push(feature_rows(q, &searched, &lifted, &placed, &closeness));
                }
                let profile = if !args.profiles || placed.iter().any(|p| p.hit.named) {
                    None
                } else {
                    profile_shown(args, &searched, searcher, cfg, pages)?
                };
                let mut rows = listed_with_pages(&lifted, placed);
                // The profile asked for ("bohemian rhapsody lyrics") is
                // shown above the results.
                if let Some(url) = profile {
                    rows.insert(0, vec![url]);
                }
                first = rows.first().and_then(|keys| keys.first()).cloned();
                let rank = rows
                    .iter()
                    .position(|keys| {
                        keys.iter()
                            .any(|k| q.expected.iter().any(|e| is_expected(e, k)))
                    })
                    .map(|i| i + 1);
                listed = Some(rows);
                rank
            }
        };
        let rank = deep_rank.filter(|&rank| rank <= args.limit);
        if verbose && args.show > 0 {
            println!("{:?}", q.query);
            let shown: Vec<String> = match &listed {
                None => domains.iter().map(|d| d.to_string()).collect(),
                Some(rows) => rows.iter().map(|keys| keys.join(" + ")).collect(),
            };
            for (i, row) in shown.iter().take(args.show).enumerate() {
                println!("  {}. {row}", i + 1);
            }
        }
        if verbose && rank != Some(1) {
            println!("{}", format_miss(q, rank, first.as_deref(), args.limit));
            if args.explain {
                let closeness = |domain: &str| {
                    query_meaning
                        .as_ref()
                        .and_then(|meaning| meaning.closeness(domain))
                };
                let shown = [Some(1), deep_rank];
                for rank in shown.into_iter().flatten() {
                    // A listed position is a site's domain or a page's address.
                    let key = match &listed {
                        None => hits.get(rank - 1).map(|h| h.domain.as_str()),
                        Some(rows) => rows
                            .get(rank - 1)
                            .and_then(|keys| keys.first())
                            .map(String::as_str),
                    };
                    let Some(key) = key else { continue };
                    match hits.iter().find(|h| h.domain == key) {
                        Some(hit) => println!("{}", explain(rank, hit, closeness(&hit.domain))),
                        None => println!("  #{rank} page {key}"),
                    }
                }
                if deep_rank.is_none() {
                    println!("  expected site not in the first {fetched}");
                }
            }
        }
        ranks.push(rank);
    }
    Ok(ranks)
}

/// One ranking to try in a sweep: a name and the knobs it changes.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub name: String,
    pub cfg: plumb_index::RankConfig,
}

/// Parses a sweep file: one `name<TAB>{"knob": value, ...}` per line, each
/// knob changed from `base` (the ranking `--rank` and `--alpha` give);
/// blank lines and lines starting with `#` are skipped. The first variant
/// is always `base` itself, which the others are compared with.
pub fn parse_sweep(text: &str, base: &plumb_index::RankConfig) -> Result<Vec<Variant>> {
    let base_json = serde_json::to_value(base)?;
    let mut variants = vec![Variant {
        name: "base".to_string(),
        cfg: *base,
    }];
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((name, knobs)) = trimmed.split_once('\t') else {
            bail!("line {line}: expected `name<TAB>{{json}}`, found no tab");
        };
        let name = name.trim();
        if variants.iter().any(|v| v.name == name) {
            bail!("line {line}: a variant is already called {name:?}");
        }
        let knobs: serde_json::Value = serde_json::from_str(knobs.trim())
            .with_context(|| format!("line {line}: reading the knobs of {name:?}"))?;
        let serde_json::Value::Object(knobs) = knobs else {
            bail!("line {line}: the knobs of {name:?} are not a JSON object");
        };
        let mut merged = base_json.clone();
        for (knob, value) in knobs {
            let serde_json::Value::Object(fields) = &mut merged else {
                unreachable!("RankConfig serializes as an object");
            };
            if !fields.contains_key(&knob) {
                bail!("line {line}: {name:?} changes {knob:?}, which is no ranking knob");
            }
            fields.insert(knob, value);
        }
        let cfg = serde_json::from_value(merged)
            .with_context(|| format!("line {line}: reading the knobs of {name:?}"))?;
        variants.push(Variant {
            name: name.to_string(),
            cfg,
        });
    }
    Ok(variants)
}

/// `--sweep`: every suite under every variant, in one table, each
/// compared query by query with `base`.
fn sweep(args: &EvalArgs, setup: &Setup, suites: &[Suite], variants: &[Variant]) -> Result<()> {
    // ranks[variant][suite][query]
    let mut ranks: Vec<Vec<Vec<Option<usize>>>> = Vec::with_capacity(variants.len());
    for variant in variants {
        info!("sweep: {} ({:?})", variant.name, variant.cfg);
        let mut of_suites = Vec::with_capacity(suites.len());
        for suite in suites {
            of_suites.push(evaluate(
                args,
                setup,
                &suite.queries,
                &variant.cfg,
                false,
                None,
            )?);
        }
        ranks.push(of_suites);
    }
    print!("{}", format_sweep(args.limit, suites, variants, &ranks));
    if let Some(path) = &args.ranks_out {
        let mut out = String::from("variant\tsuite\tline\tquery\trank\n");
        for (v, variant) in variants.iter().enumerate() {
            for (s, suite) in suites.iter().enumerate() {
                for (q, query) in suite.queries.iter().enumerate() {
                    let rank = ranks[v][s][q].unwrap_or(0);
                    let _ = writeln!(
                        out,
                        "{}\t{}\t{}\t{}\t{rank}",
                        variant.name, suite.name, query.line, query.query
                    );
                }
            }
        }
        std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// A rank as a number to compare: lower is better, not found is worst.
fn place(rank: Option<usize>) -> usize {
    rank.unwrap_or(usize::MAX)
}

fn format_sweep(
    limit: usize,
    suites: &[Suite],
    variants: &[Variant],
    ranks: &[Vec<Vec<Option<usize>>>],
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<24} {:<22} {:>7} {:>7} {:>7} {:>6} {:>6}",
        "variant", "suite", "top-1", "top-3", "MRR", "better", "worse"
    );
    for (v, variant) in variants.iter().enumerate() {
        let mut all = Vec::new();
        let (mut better, mut worse) = (0, 0);
        for (s, suite) in suites.iter().enumerate() {
            let these = &ranks[v][s];
            let base = &ranks[0][s];
            let b = these
                .iter()
                .zip(base)
                .filter(|(r, b)| place(**r) < place(**b))
                .count();
            let w = these
                .iter()
                .zip(base)
                .filter(|(r, b)| place(**r) > place(**b))
                .count();
            better += b;
            worse += w;
            all.extend_from_slice(these);
            let m = Metrics::from_ranks(these, limit);
            let _ = writeln!(
                out,
                "{:<24} {:<22} {:>6.1}% {:>6.1}% {:>7.3} {:>6} {:>6}",
                variant.name,
                suite.name,
                m.top1_rate() * 100.0,
                m.top3_rate() * 100.0,
                m.mrr,
                b,
                w
            );
        }
        let m = Metrics::from_ranks(&all, limit);
        let _ = writeln!(
            out,
            "{:<24} {:<22} {:>6.1}% {:>6.1}% {:>7.3} {:>6} {:>6}",
            variant.name,
            "ALL",
            m.top1_rate() * 100.0,
            m.top3_rate() * 100.0,
            m.mrr,
            better,
            worse
        );
    }
    // What each variant changed, query by query.
    let shown = |rank: Option<usize>| rank.map_or_else(|| "-".to_string(), |r| r.to_string());
    for (v, variant) in variants.iter().enumerate().skip(1) {
        let mut changed = Vec::new();
        for (s, suite) in suites.iter().enumerate() {
            for (q, query) in suite.queries.iter().enumerate() {
                let (was, now) = (ranks[0][s][q], ranks[v][s][q]);
                if was != now {
                    changed.push(format!(
                        "  {} {:?}: {} -> {}",
                        suite.name,
                        query.query,
                        shown(was),
                        shown(now)
                    ));
                }
            }
        }
        if !changed.is_empty() {
            let _ = writeln!(out, "\n{} changed:", variant.name);
            for line in changed {
                let _ = writeln!(out, "{line}");
            }
        }
    }
    out
}

/// The address of the profile a node shows above the results for `query`
/// ("bohemian rhapsody lyrics", "mrbeast youtube"), found as a node finds
/// it, by searching for the words before the service.
fn profile_shown(
    args: &EvalArgs,
    query: &str,
    searcher: &Searcher,
    cfg: &plumb_index::RankConfig,
    pages: &PageSearcher,
) -> Result<Option<String>> {
    let options = SearchOptions {
        country: args.country.clone(),
        language: args.lang.clone(),
        ..SearchOptions::default()
    };
    for name in crate::web::answers::profile_lookups(query) {
        let mut sites = searcher.search_meaning(&name, 5, cfg, &options, None)?;
        pages.note_demand(&mut sites.hits)?;
        let found = pages.search(&name, 10)?;
        let placed = place_pages(&name, &sites.hits, found);
        if let Some(profile) = crate::web::answers::profile_answer(query, &placed) {
            return Ok(Some(profile.url));
        }
    }
    Ok(None)
}

/// With `--facts`: `Some(1)` when the instant answer to `q` (worked out
/// as a node does, by searching for the fact's subject) has one of the
/// expected texts, commas left out ("8848" in "8,848.86 m"); `None`
/// otherwise.
fn fact_rank(
    args: &EvalArgs,
    q: &EvalQuery,
    searcher: &Searcher,
    cfg: &plumb_index::RankConfig,
    pages: Option<&PageSearcher>,
    verbose: bool,
) -> Result<Option<usize>> {
    let asked = plumb_core::facts::fact_asked(&q.query);
    let answer = match (&asked, pages) {
        (Some(asked), Some(pages)) => {
            let options = SearchOptions {
                country: args.country.clone(),
                exact: true,
                language: args.lang.clone(),
                ..SearchOptions::default()
            };
            let mut sites = searcher.search_meaning(&asked.subject, 5, cfg, &options, None)?;
            pages.note_demand(&mut sites.hits)?;
            let found = pages.search(&asked.subject, 10)?;
            let placed = place_pages(&asked.subject, &sites.hits, found);
            crate::web::answers::fact_answer(asked, &placed, plumb_core::now_unix())
        }
        _ => None,
    };
    let text = answer.as_ref().map(|a| {
        format!(
            "{}: {} ({})",
            a.question,
            a.answer,
            a.note.as_deref().unwrap_or("")
        )
    });
    let hit = answer.as_ref().is_some_and(|a| {
        let shown = a.answer.to_lowercase().replace(',', "");
        q.expected.iter().any(|e| shown.contains(e.as_str()))
    });
    if verbose && (args.show > 0 || !hit) {
        let what = match (&asked, &text) {
            (None, _) => "not read as a fact question".to_string(),
            (Some(_), None) => "no answer".to_string(),
            (Some(_), Some(text)) => text.clone(),
        };
        let mark = if hit { "ok" } else { "miss" };
        println!(
            "{mark}: {:?} expected {}; {what}",
            q.query,
            q.expected.join(" or ")
        );
    }
    Ok(hit.then_some(1))
}

/// What each position of a results page holds, as a node lists sites and
/// pages: a page's address, or a site's domain with the addresses of the
/// pages shown under it.
fn listed_with_pages(hits: &[Hit], pages: Vec<PlacedPage>) -> Vec<Vec<String>> {
    let mut listed = Vec::new();
    let alone = |at: usize| {
        pages
            .iter()
            .filter(move |p| p.under.is_none() && p.at == at)
            .map(|p| vec![p.hit.page.url.clone()])
    };
    for (i, hit) in hits.iter().enumerate() {
        listed.extend(alone(i));
        let mut keys = vec![hit.domain.clone()];
        keys.extend(
            pages
                .iter()
                .filter(|p| p.under.as_deref() == Some(hit.domain.as_str()))
                .map(|p| p.hit.page.url.clone()),
        );
        listed.push(keys);
    }
    listed.extend(
        pages
            .iter()
            .filter(|p| p.under.is_none() && p.at >= hits.len())
            .map(|p| vec![p.hit.page.url.clone()]),
    );
    listed
}

/// Most listed rows of a query `--features-out` writes.
const FEATURE_ROWS: usize = 30;

/// One query's listed rows with everything that ranked them, for
/// `--features-out`: what a learned ranking is trained and judged on.
/// Rows are listed as [`listed_with_pages`] lists them; `label` is 1 for
/// a row holding an expected answer.
fn feature_rows(
    q: &EvalQuery,
    searched: &str,
    hits: &[Hit],
    placed: &[PlacedPage],
    closeness: &dyn Fn(&str) -> Option<f32>,
) -> serde_json::Value {
    use serde_json::json;
    let expected = |key: &str| q.expected.iter().any(|e| is_expected(e, key));
    let page = |p: &PlacedPage| {
        json!({
            "key": p.hit.page.url,
            "set": p.hit.page.set,
            "title": p.hit.page.title,
            "description": p.hit.page.description,
            "score": p.hit.score,
            "named": p.hit.named,
            "popularity": p.hit.popularity,
            "whole": p.hit.whole,
            "label": u8::from(expected(&p.hit.page.url)),
        })
    };
    let alone = |at: usize| {
        placed
            .iter()
            .filter(move |p| p.under.is_none() && p.at == at)
            .map(|p| {
                let mut row = page(p);
                row["kind"] = "page".into();
                row
            })
    };
    let mut rows = Vec::new();
    for (i, hit) in hits.iter().enumerate() {
        rows.extend(alone(i));
        let under: Vec<serde_json::Value> = placed
            .iter()
            .filter(|p| p.under.as_deref() == Some(hit.domain.as_str()))
            .map(page)
            .collect();
        let label = expected(&hit.domain) || under.iter().any(|p| p["label"] == 1);
        rows.push(json!({
            "kind": "site",
            "key": hit.domain,
            "site_rank": i + 1,
            "title": hit.title,
            "description": hit.description,
            "score": hit.score,
            "text_score": hit.text_score,
            "placing_text_score": hit.placing_text_score,
            "link_score": hit.link_score,
            "closeness": closeness(&hit.domain),
            "country": hit.country,
            "named": hit.named,
            "official": hit.official,
            "demand": hit.demand,
            "under": under,
            "label": u8::from(label),
        }));
    }
    rows.extend(
        placed
            .iter()
            .filter(|p| p.under.is_none() && p.at >= hits.len())
            .map(|p| {
                let mut row = page(p);
                row["kind"] = "page".into();
                row
            }),
    );
    rows.truncate(FEATURE_ROWS);
    json!({
        "line": q.line,
        "query": q.query,
        "searched": searched,
        "half": match half_of(&q.query) {
            Half::Tune => "tune",
            Half::HeldOut => "held-out",
        },
        "expected": q.expected,
        "rows": rows,
    })
}

/// Most rows of a query each `--rerank-model` scores.
const RERANKED_ROWS: usize = 20;

/// What a reranker reads of a listed row: a site's name, title and
/// description, or a page's title and description.
fn row_text(row: &serde_json::Value) -> String {
    let field = |name: &str| row[name].as_str().unwrap_or("").trim().to_string();
    let mut text = field("title");
    if row["kind"] == "site" {
        let key = field("key");
        text = if text.is_empty() {
            key
        } else {
            format!("{text} ({key})")
        };
    }
    let description = field("description");
    if !description.is_empty() {
        text = format!("{text}. {description}");
    }
    text
}

/// Scores the first [`RERANKED_ROWS`] rows of a query written by
/// [`feature_rows`] with each reranker: `ce` holds one score per model,
/// and the query's `ce_ms` how long each model took over all its rows.
fn rerank_rows(rerankers: &[plumb_embed::Reranker], query: &mut serde_json::Value) -> Result<()> {
    if rerankers.is_empty() {
        return Ok(());
    }
    let searched = query["searched"].as_str().unwrap_or("").to_string();
    let mut millis = Vec::with_capacity(rerankers.len());
    let Some(rows) = query["rows"].as_array_mut() else {
        return Ok(());
    };
    for row in rows.iter_mut() {
        row["ce"] = serde_json::json!([]);
    }
    for reranker in rerankers {
        let start = std::time::Instant::now();
        for row in rows.iter_mut().take(RERANKED_ROWS) {
            let score = reranker.score(&searched, &row_text(row))?;
            if let Some(scores) = row["ce"].as_array_mut() {
                scores.push(score.into());
            }
        }
        millis.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    query["ce_ms"] = millis.into();
    Ok(())
}

/// How deep `--explain` looks for the expected site.
const EXPLAIN_DEPTH: usize = 1_000;

/// How the site at `rank` scored, for `--explain`.
fn explain(rank: usize, hit: &Hit, closeness: Option<f32>) -> String {
    let closeness = closeness.map_or_else(|| "-".to_string(), |c| format!("{c:.3}"));
    format!(
        "  #{rank} {}: score {:.3}, text {:.3}, link {:.3}, closeness {closeness}",
        hit.domain, hit.score, hit.text_score, hit.link_score
    )
}

/// One line describing a query whose expected site did not come first.
fn format_miss(q: &EvalQuery, rank: Option<usize>, first: Option<&str>, limit: usize) -> String {
    let found = match rank {
        Some(rank) => format!("found at rank {rank}"),
        None => format!("not found in the top {limit}"),
    };
    let first = match first {
        Some(domain) => format!("first was {domain}"),
        None => "no results".to_string(),
    };
    format!(
        "miss: {:?} (line {}) expected {}, {found}; {first}",
        q.query,
        q.line,
        q.expected.join(" or ")
    )
}

fn format_totals(m: &Metrics, limit: usize) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "queries  {}", m.queries);
    let _ = writeln!(
        out,
        "top-1    {:.1}%  ({}/{})",
        m.top1_rate() * 100.0,
        m.top1,
        m.queries
    );
    let _ = writeln!(
        out,
        "top-3    {:.1}%  ({}/{})",
        m.top3_rate() * 100.0,
        m.top3,
        m.queries
    );
    let _ = writeln!(out, "MRR@{limit:<4} {:.3}", m.mrr);
    out
}

/// The page set a file is of, from its name, which starts with the set's:
/// github.tsv.gz and github-new.tsv.gz are repositories,
/// wikipedia-en-old.tsv.gz English Wikipedia; any other is English
/// Wikipedia.
pub(crate) fn set_of_file(name: &str) -> String {
    use plumb_index::pages::{
        BOOKS_SET, DOCS_SET, FILMS_SET, GITHUB_SET, MUSIC_SET, PACKAGES_SET, PAPERS_SET,
        PODCASTS_SET, STACKEXCHANGE_SET, STACKOVERFLOW_SET, WIKIDATA_SET,
    };
    let stem = name.split('.').next().unwrap_or("");
    if let Some(set) = [
        GITHUB_SET,
        STACKOVERFLOW_SET,
        STACKEXCHANGE_SET,
        BOOKS_SET,
        PAPERS_SET,
        PACKAGES_SET,
        PODCASTS_SET,
        MUSIC_SET,
        FILMS_SET,
        DOCS_SET,
        WIKIDATA_SET,
    ]
    .into_iter()
    .find(|set| stem.starts_with(set))
    {
        return set.to_string();
    }
    let lang = stem
        .strip_prefix("wikipedia-")
        .and_then(|rest| rest.split('-').next())
        .filter(|lang| !lang.is_empty())
        .unwrap_or("en");
    format!("wikipedia-{lang}")
}

#[cfg(test)]
mod tests {
    use super::{half_of, is_expected, parse_queries, parse_sweep, rank_of, set_of_file};
    use crate::cli::Half;

    #[test]
    fn a_sweep_changes_knobs_of_the_base_ranking() {
        let base = plumb_index::RankConfig {
            alpha: 0.3,
            ..Default::default()
        };
        let text = "# knobs\nlabel\t{\"exact_label_bonus\": 0.1}\nnone\t{\"named_share\": null}\n";
        let variants = parse_sweep(text, &base).unwrap();
        let names: Vec<&str> = variants.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["base", "label", "none"]);
        assert_eq!(variants[0].cfg, base);
        assert_eq!(variants[1].cfg.exact_label_bonus, 0.1);
        assert_eq!(variants[1].cfg.alpha, 0.3);
        assert_eq!(variants[2].cfg.named_share, None);
        assert!(parse_sweep("x\t{\"no_such_knob\": 1}", &base).is_err());
        assert!(parse_sweep("x {}", &base).is_err());
        assert!(parse_sweep("x\t{}\nx\t{}", &base).is_err());
    }

    #[test]
    fn sets_come_from_file_names() {
        assert_eq!(set_of_file("github.tsv.gz"), "github");
        assert_eq!(set_of_file("github-new.tsv.gz"), "github");
        assert_eq!(set_of_file("stackoverflow.tsv.gz"), "stackoverflow");
        assert_eq!(set_of_file("stackexchange.tsv.gz"), "stackexchange");
        assert_eq!(set_of_file("books.tsv"), "books");
        assert_eq!(set_of_file("packages.tsv.gz"), "packages");
        assert_eq!(set_of_file("podcasts.tsv.gz"), "podcasts");
        assert_eq!(set_of_file("wikipedia-de.tsv.gz"), "wikipedia-de");
        assert_eq!(set_of_file("wikipedia-en-before157.tsv.gz"), "wikipedia-en");
        assert_eq!(set_of_file("articles.tsv.gz"), "wikipedia-en");
    }

    #[test]
    fn an_address_ending_in_a_star_takes_the_pages_it_starts() {
        let queries =
            parse_queries("unclog a drain\thttps://diy.stackexchange.com/questions/*\n").unwrap();
        let expected = &queries[0].expected;
        assert_eq!(expected, &["https://diy.stackexchange.com/questions/*"]);
        let rows = [
            "drain.com",
            "https://superuser.com/questions/1",
            "https://diy.stackexchange.com/questions/2142",
        ];
        assert_eq!(rank_of(&rows, expected), Some(3));
        assert!(is_expected("usbank.com", "usbank.com"));
        assert!(!is_expected("usbank.com", "usbank.com.evil"));
    }

    #[test]
    fn pages_are_listed_as_a_node_lists_them() {
        let site = |domain: &str, named: bool| Hit {
            demand: None,
            missing_words: false,
            placing_text_score: None,
            domain: domain.into(),
            url: format!("https://{domain}/"),
            title: None,
            description: None,
            score: 1.0,
            text_score: 1.0,
            link_score: 0.5,
            country: None,
            named,
            official: false,
            key_pages: Vec::new(),
        };
        let page = |title: &str, site: Option<&str>| plumb_index::pages::PageHit {
            page: Page {
                site: site.map(str::to_string),
                ..Page::from_article(
                    "en",
                    plumb_core::Article {
                        title: title.into(),
                        ..Default::default()
                    },
                )
            },
            score: 0.9,
            named: true,
            popularity: 0.9,
            whole: false,
            learned: None,
        };
        let hits = [site("curie.org", false), site("python.org", false)];
        let placed = place_pages(
            "",
            &hits,
            vec![
                page("Marie Curie", None),
                page("Python", Some("python.org")),
            ],
        );
        let listed = listed_with_pages(&hits, placed);
        assert_eq!(
            listed,
            [
                vec!["https://en.wikipedia.org/wiki/Marie_Curie".to_string()],
                vec!["curie.org".to_string()],
                vec![
                    "python.org".to_string(),
                    "https://en.wikipedia.org/wiki/Python".to_string()
                ],
            ]
        );
        let queries =
            parse_queries("marie curie\thttps://en.wikipedia.org/wiki/Marie_Curie\n").unwrap();
        assert_eq!(
            queries[0].expected,
            ["https://en.wikipedia.org/wiki/Marie_Curie"]
        );
    }
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn a_byte_order_mark_is_skipped() {
        let queries = parse_queries("\u{feff}# saved by Notepad\nus bank\tusbank.com\n").unwrap();
        assert_eq!(queries.len(), 1);
        let queries = parse_queries("\u{feff}us bank\tusbank.com\n").unwrap();
        assert_eq!(queries[0].query, "us bank");
    }

    #[test]
    fn parses_queries_comments_and_alternatives() {
        let text = "# brand queries\n\
                    \n\
                    us bank\tusbank.com\n\
                    \x20 # indented comment\n\
                    BBC\tbbc.co.uk, https://www.bbc.com/\r\n\
                    wells fargo \t WWW.WellsFargo.com.\n";
        let queries = parse_queries(text).unwrap();
        assert_eq!(
            queries,
            vec![
                EvalQuery {
                    line: 3,
                    query: "us bank".into(),
                    expected: vec!["usbank.com".into()],
                },
                EvalQuery {
                    line: 5,
                    query: "BBC".into(),
                    expected: vec!["bbc.co.uk".into(), "bbc.com".into()],
                },
                EvalQuery {
                    line: 6,
                    query: "wells fargo".into(),
                    expected: vec!["wellsfargo.com".into()],
                },
            ]
        );
        assert!(parse_queries("# nothing here\n\n").unwrap().is_empty());
    }

    #[test]
    fn rejects_malformed_lines() {
        let err = parse_queries("ok\tok.com\nus bank usbank.com\n").unwrap_err();
        assert!(err.to_string().contains("line 2"), "{err}");
        assert!(parse_queries("\tusbank.com\n").is_err());
        assert!(parse_queries("us bank\t , \n").is_err());
    }

    #[test]
    fn ranks_first_acceptable_domain() {
        let results = ["chasecenter.com", "chase.com", "bbc.com"];
        assert_eq!(rank_of(&results, &["chase.com".into()]), Some(2));
        assert_eq!(
            rank_of(&results, &["bbc.co.uk".into(), "bbc.com".into()]),
            Some(3)
        );
        assert_eq!(rank_of(&results, &["usbank.com".into()]), None);
        assert_eq!(rank_of::<&str>(&[], &["usbank.com".into()]), None);
    }

    #[test]
    fn metrics_from_ranks() {
        let m = Metrics::from_ranks(&[Some(1), Some(1), Some(2), Some(3), Some(5), None], 10);
        assert_eq!(m.queries, 6);
        assert_eq!(m.top1, 2);
        assert_eq!(m.top3, 4);
        let expected_mrr = (1.0 + 1.0 + 0.5 + 1.0 / 3.0 + 0.2) / 6.0;
        assert!(close(m.mrr, expected_mrr), "{}", m.mrr);
        assert!(close(m.top1_rate(), 2.0 / 6.0));
        assert!(close(m.top3_rate(), 4.0 / 6.0));
    }

    #[test]
    fn metrics_respect_the_limit() {
        // With only two results per query, rank 3 cannot count.
        let m = Metrics::from_ranks(&[Some(1), Some(3)], 2);
        assert_eq!((m.top1, m.top3), (1, 1));
        assert!(close(m.mrr, 0.5));
        let m = Metrics::from_ranks(&[Some(1)], 1);
        assert!(close(m.mrr, 1.0) && close(m.top3_rate(), 1.0));
    }

    #[test]
    fn metrics_of_nothing() {
        let m = Metrics::from_ranks(&[], 10);
        assert_eq!((m.queries, m.top1, m.top3), (0, 0, 0));
        assert!(close(m.mrr, 0.0) && close(m.top1_rate(), 0.0));
        let all_missed = Metrics::from_ranks(&[None, None], 10);
        assert!(close(all_missed.mrr, 0.0) && close(all_missed.top3_rate(), 0.0));
    }

    #[test]
    fn halves_depend_on_the_words_alone() {
        // Fixed for good: tuning runs on other machines rely on them.
        assert_eq!(half_of("chase"), Half::Tune);
        assert_eq!(half_of("paypal"), Half::HeldOut);
        assert_eq!(half_of("marie curie"), Half::Tune);
        assert_eq!(half_of("  Marie   CURIE "), half_of("marie curie"));
        let halves: Vec<Half> = (0..1000).map(|i| half_of(&format!("query {i}"))).collect();
        let tune = halves.iter().filter(|h| **h == Half::Tune).count();
        assert!(
            (400..=600).contains(&tune),
            "{tune} of 1000 in the tune half"
        );
    }

    /// Every queries file in eval/ parses, has no query twice, and writes
    /// each expected answer as hits are keyed: a registrable domain, or a
    /// page's https address. Both halves of each file have queries.
    #[test]
    fn repository_query_files_are_valid() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(root.join("eval"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|e| e == "tsv"))
            .collect();
        files.sort();
        assert!(files.len() >= 11, "{files:?}");
        files.push(root.join("fixtures/brand_queries.tsv"));
        for path in files {
            let file = path.display();
            let text = std::fs::read_to_string(&path).unwrap();
            let queries = parse_queries(&text).unwrap_or_else(|err| panic!("{file}: {err:#}"));
            assert!(
                queries.len() >= 40,
                "{file}: only {} queries",
                queries.len()
            );
            let mut seen = std::collections::HashSet::new();
            for q in &queries {
                let words = q.query.split_whitespace().collect::<Vec<_>>().join(" ");
                assert!(
                    seen.insert(words.to_lowercase()),
                    "{file}: {:?} twice",
                    q.query
                );
            }
            let held_out = queries
                .iter()
                .filter(|q| half_of(&q.query) == Half::HeldOut)
                .count();
            assert!(
                held_out * 4 >= queries.len() && held_out * 4 <= queries.len() * 3,
                "{file}: {held_out} of {} held out",
                queries.len()
            );
            let facts = path.ends_with("fact_queries.tsv");
            for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
                let Some((_, answers)) = line.split_once('\t') else {
                    continue;
                };
                for answer in answers.split(',') {
                    if facts {
                        assert_eq!(answer, answer.trim().to_lowercase(), "{file}: {line:?}");
                    } else if answer.contains('/') {
                        let url = url::Url::parse(answer.trim_end_matches('*'));
                        assert!(
                            url.is_ok_and(|u| u.scheme() == "https" && u.path() != "/"),
                            "{file}: {line:?}"
                        );
                    } else {
                        assert_eq!(
                            registrable_domain(answer).as_deref(),
                            Some(answer),
                            "{file}: {line:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn formats_misses_and_totals() {
        let q = EvalQuery {
            line: 7,
            query: "chase".into(),
            expected: vec!["chase.com".into()],
        };
        assert_eq!(
            format_miss(&q, Some(2), Some("chasecenter.com"), 10),
            "miss: \"chase\" (line 7) expected chase.com, found at rank 2; first was chasecenter.com"
        );
        assert_eq!(
            format_miss(&q, None, None, 10),
            "miss: \"chase\" (line 7) expected chase.com, not found in the top 10; no results"
        );
        let totals = format_totals(&Metrics::from_ranks(&[Some(1), Some(2), None], 10), 10);
        assert_eq!(
            totals,
            "queries  3\ntop-1    33.3%  (1/3)\ntop-3    66.7%  (2/3)\nMRR@10   0.500\n"
        );
    }
}
