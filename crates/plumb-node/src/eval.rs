//! `plumb eval`: the brand-name test. For each query in a TSV file, where
//! does the expected official site rank?
//!
//! The file has one `query<TAB>expected_domain[,another_ok_domain]` per
//! line; blank lines and lines starting with `#` are skipped. An expected
//! answer can also be a page's address (`https://en.wikipedia.org/wiki/
//! Marie_Curie`): with `--pages`, pages are listed among the sites as a
//! node lists them (see [`plumb_index::pages::place_pages`]), and a page
//! shown under a site's result counts at that site's rank. The metrics
//! are top-1 and top-3 rates and the mean reciprocal rank within the
//! results fetched ([`Metrics::from_ranks`]).

use std::fmt::Write as _;

use anyhow::{bail, Context, Result};
use plumb_core::registrable_domain;
use plumb_index::pages::{place_pages, Page, PageSearcher, PlacedPage};
use plumb_index::{Hit, Meaning, SearchOptions, Searcher};
use tracing::info;

use crate::cli::EvalArgs;
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

fn normalize_domain(domain: &str) -> String {
    // A page's address stays one; a homepage's counts as its site.
    if let Ok(url) = url::Url::parse(domain) {
        if matches!(url.scheme(), "http" | "https") && url.path() != "/" {
            return domain.to_string();
        }
    }
    registrable_domain(domain).unwrap_or_else(|| domain.trim_end_matches('.').to_ascii_lowercase())
}

/// 1-based position of the first result whose domain is one of `expected`.
pub fn rank_of<S: AsRef<str>>(results: &[S], expected: &[String]) -> Option<usize> {
    results
        .iter()
        .position(|domain| expected.iter().any(|e| e == domain.as_ref()))
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

pub fn run(args: EvalArgs) -> Result<()> {
    let text = std::fs::read_to_string(&args.queries)
        .with_context(|| format!("reading {}", args.queries.display()))?;
    let queries =
        parse_queries(&text).with_context(|| format!("parsing {}", args.queries.display()))?;
    if queries.is_empty() {
        bail!("{} has no queries", args.queries.display());
    }
    let searcher = Searcher::open(&args.index)
        .with_context(|| format!("opening the index in {}", args.index.display()))?;
    let mut cfg = args.rank.unwrap_or_default();
    if let Some(alpha) = args.alpha {
        cfg.alpha = alpha;
    }
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
            // Files are named after their set: github.tsv.gz,
            // wikipedia-en.tsv.gz.
            let set = name.split('.').next().unwrap_or("");
            let set = if Page::from_set(set, Default::default()).is_some() {
                set
            } else {
                "wikipedia-en"
            };
            all.extend(articles.into_iter().filter_map(|a| Page::from_set(set, a)));
        }
        plumb_index::pages::build_page_index(pages_dir.path(), all)?;
        Some(PageSearcher::open(pages_dir.path())?)
    };
    info!(
        "evaluating {} queries against {} sites ({cfg:?})",
        queries.len(),
        searcher.num_docs(),
    );
    // With --explain, sites ranked below the limit are fetched too, to show
    // how far behind the expected one is.
    let fetched = if args.explain {
        args.limit.max(EXPLAIN_DEPTH)
    } else {
        args.limit
    };

    let mut ranks = Vec::with_capacity(queries.len());
    for q in &queries {
        let options = SearchOptions {
            country: args.country.clone(),
            only_country: false,
            exact: args.exact,
            ..SearchOptions::default()
        };
        let query_meaning = meaning.as_ref().and_then(|meaning| meaning.query(&q.query));
        let hits = searcher
            .search_meaning(
                &q.query,
                fetched,
                &cfg,
                &options,
                query_meaning
                    .as_ref()
                    .map(|m| m as &dyn plumb_index::Meaning),
            )
            .with_context(|| format!("searching for {:?}", q.query))?
            .hits;
        let domains: Vec<&str> = hits.iter().map(|h| h.domain.as_str()).collect();
        // What came first: a page when one was listed first.
        let mut first = domains.first().map(|d| d.to_string());
        // With pages, what each listed position holds (pages count as rows).
        let mut listed = None;
        let deep_rank = match &pages {
            None => rank_of(&domains, &q.expected),
            Some(pages) => {
                let found = pages
                    .search(&q.query, 10)
                    .with_context(|| format!("searching pages for {:?}", q.query))?;
                let rows = listed_with_pages(&hits, place_pages(&q.query, &hits, found));
                first = rows.first().and_then(|keys| keys.first()).cloned();
                let rank = rows
                    .iter()
                    .position(|keys| keys.iter().any(|k| q.expected.contains(k)))
                    .map(|i| i + 1);
                listed = Some(rows);
                rank
            }
        };
        let rank = deep_rank.filter(|&rank| rank <= args.limit);
        if rank != Some(1) {
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

    let metrics = Metrics::from_ranks(&ranks, args.limit);
    print!("{}", format_totals(&metrics, args.limit));
    if let Some(min) = args.min_top1 {
        if metrics.top1_rate() < min {
            bail!(
                "top-1 is {:.1}%, below --min-top1 {:.1}%",
                metrics.top1_rate() * 100.0,
                min * 100.0
            );
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {

    #[test]
    fn pages_are_listed_as_a_node_lists_them() {
        let site = |domain: &str, named: bool| Hit {
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

    /// The query lists in the repository parse, and every expected domain is
    /// written as the registrable domain that hits are keyed by.
    #[test]
    fn repository_query_files_are_valid() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for file in [
            "fixtures/brand_queries.tsv",
            "eval/brand_queries.tsv",
            "eval/ai_queries.tsv",
        ] {
            let text = std::fs::read_to_string(root.join(file)).unwrap();
            let queries = parse_queries(&text).unwrap();
            assert!(
                queries.len() >= 40,
                "{file}: only {} queries",
                queries.len()
            );
            for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
                let Some((_, domains)) = line.split_once('\t') else {
                    continue;
                };
                for domain in domains.split(',') {
                    assert_eq!(
                        registrable_domain(domain).as_deref(),
                        Some(domain),
                        "{file}: {line:?}"
                    );
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
