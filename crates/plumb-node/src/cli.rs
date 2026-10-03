//! Command-line arguments of the `plumb` binary.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand};

/// Plumb Search: a self-hostable search engine that finds sites by name.
///
/// Typical first run: fetch-data, ingest, crawl (optional), index, then
/// search, serve or eval.
#[derive(Debug, Parser)]
#[command(name = "plumb", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Download the seed datasets: Tranco, Common Crawl domain ranks and
    /// Wikidata official websites.
    FetchData(FetchDataArgs),
    /// Fold seed data and earlier records into one records file.
    Ingest(IngestArgs),
    /// Fetch the homepages of the best-scored records and merge what they say.
    Crawl(CrawlArgs),
    /// Build the search index from a records file.
    Index(IndexArgs),
    /// Search the index from the command line.
    Search(SearchArgs),
    /// Serve the search page and a JSON API over HTTP.
    Serve(ServeArgs),
    /// Check how often the official site ranks first for a list of queries.
    Eval(EvalArgs),
}

#[derive(Debug, Args)]
pub struct FetchDataArgs {
    /// Directory to download into (created if missing).
    #[arg(long, value_name = "DIR")]
    pub dir: PathBuf,
    /// Common Crawl web graph release to take domain ranks from, such as
    /// cc-main-2025-26-nov-dec-jan (release names are listed on
    /// https://commoncrawl.org/web-graphs). Without this or --cc-ranks-url,
    /// Common Crawl is skipped.
    #[arg(long, value_name = "NAME", conflicts_with = "cc_ranks_url")]
    pub cc_release: Option<String>,
    /// Download Common Crawl domain ranks from this URL instead of a release name.
    #[arg(long, value_name = "URL")]
    pub cc_ranks_url: Option<String>,
    /// Do not download the Tranco list.
    #[arg(long)]
    pub skip_tranco: bool,
    /// Do not query Wikidata.
    #[arg(long)]
    pub skip_wikidata: bool,
    /// Only fetch Wikidata items with at least this many Wikipedia sitelinks
    /// (a notability filter that keeps the query small enough to finish).
    #[arg(long, value_name = "N", default_value_t = 25)]
    pub wikidata_min_sitelinks: u32,
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("sources")
        .required(true)
        .multiple(true)
        .args(["tranco", "cc_ranks", "wat", "wikidata", "records"])
))]
pub struct IngestArgs {
    /// Tranco list: the .zip as downloaded, a .csv or a .csv.gz.
    #[arg(long, value_name = "PATH")]
    pub tranco: Option<PathBuf>,
    /// Common Crawl domain ranks file (.txt or .txt.gz).
    #[arg(long, value_name = "PATH")]
    pub cc_ranks: Option<PathBuf>,
    /// Common Crawl WAT files, gzipped or plain (list several, or repeat the flag).
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub wat: Vec<PathBuf>,
    /// Wikidata official websites (the TSV that fetch-data writes).
    #[arg(long, value_name = "PATH")]
    pub wikidata: Option<PathBuf>,
    /// Records files from an earlier ingest or crawl to merge in (list several,
    /// or repeat the flag).
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub records: Vec<PathBuf>,
    /// Read at most N entries from the Tranco list, the Common Crawl ranks and
    /// each records file (all sorted best first). WAT and Wikidata files are
    /// always read whole.
    #[arg(long, value_name = "N")]
    pub limit_per_source: Option<usize>,
    /// Keep only the N records with the best link score.
    #[arg(long, value_name = "N")]
    pub top: Option<usize>,
    /// Where to write the records (JSON lines).
    #[arg(long, value_name = "PATH")]
    pub out: PathBuf,
}

#[derive(Debug, Args)]
pub struct CrawlArgs {
    /// Records file to pick homepages from (JSON lines).
    #[arg(long, value_name = "PATH")]
    pub records: PathBuf,
    /// How many homepages to fetch: the records with the best link score
    /// among those not fetched or tried recently.
    #[arg(long, value_name = "N", default_value_t = 1000)]
    pub top: usize,
    /// Leave out records whose homepage was fetched, or tried and failed,
    /// within this many days.
    #[arg(long, value_name = "D", default_value_t = 30)]
    pub skip_crawled_within_days: u64,
    /// Homepage fetches in flight at once.
    #[arg(long, value_name = "N", default_value_t = 16, value_parser = parse_positive)]
    pub concurrency: usize,
    /// Where to write the updated records, saved after every batch of
    /// homepages so an interrupted crawl keeps what it fetched
    /// [default: overwrite --records].
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct IndexArgs {
    /// Records file to index (JSON lines).
    #[arg(long, value_name = "PATH")]
    pub records: PathBuf,
    /// Index directory; an index already there is replaced.
    #[arg(long, value_name = "DIR")]
    pub index: PathBuf,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// Index directory.
    #[arg(long, value_name = "DIR")]
    pub index: PathBuf,
    /// Number of results.
    #[arg(long, value_name = "N", default_value_t = 10, value_parser = parse_positive)]
    pub limit: usize,
    /// Weight of the popularity prior in the ranking, from 0 to 1
    /// [default: the index's default].
    #[arg(long, value_name = "A", value_parser = parse_alpha)]
    pub alpha: Option<f32>,
    /// Print the hits as JSON.
    #[arg(long)]
    pub json: bool,
    /// What to search for, e.g. `us bank`.
    #[arg(required = true, value_name = "QUERY")]
    pub query: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Index directory.
    #[arg(long, value_name = "DIR")]
    pub index: PathBuf,
    /// Address to listen on.
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:8080")]
    pub bind: SocketAddr,
    /// Weight of the popularity prior in the ranking, from 0 to 1
    /// [default: the index's default].
    #[arg(long, value_name = "A", value_parser = parse_alpha)]
    pub alpha: Option<f32>,
}

#[derive(Debug, Args)]
pub struct EvalArgs {
    /// Index directory.
    #[arg(long, value_name = "DIR")]
    pub index: PathBuf,
    /// Queries file: `query<TAB>expected_domain[,another_ok_domain]` per line;
    /// blank lines and lines starting with `#` are skipped.
    #[arg(long, value_name = "TSV")]
    pub queries: PathBuf,
    /// Results fetched per query; an expected site further down counts as not found.
    #[arg(long, value_name = "N", default_value_t = 10, value_parser = parse_positive)]
    pub limit: usize,
    /// Weight of the popularity prior in the ranking, from 0 to 1
    /// [default: the index's default].
    #[arg(long, value_name = "A", value_parser = parse_alpha)]
    pub alpha: Option<f32>,
    /// Exit with an error when the share of queries answered at rank 1 is
    /// below this fraction, e.g. 0.9.
    #[arg(long, value_name = "F", value_parser = parse_fraction)]
    pub min_top1: Option<f64>,
}

fn parse_positive(s: &str) -> Result<usize, String> {
    match s.trim().parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("expected a whole number above 0, got `{s}`")),
    }
}

fn parse_alpha(s: &str) -> Result<f32, String> {
    match s.trim().parse::<f32>() {
        Ok(a) if (0.0..=1.0).contains(&a) => Ok(a),
        _ => Err(format!("expected a number from 0 to 1, got `{s}`")),
    }
}

fn parse_fraction(s: &str) -> Result<f64, String> {
    match s.trim().parse::<f64>() {
        Ok(f) if (0.0..=1.0).contains(&f) => Ok(f),
        _ => Err(format!(
            "expected a fraction from 0 to 1 (0.9 means 90%), got `{s}`"
        )),
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("plumb").chain(args.iter().copied()))
    }

    #[test]
    fn definitions_are_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn ingest_needs_a_source() {
        let err = parse(&["ingest", "--out", "r.jsonl"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        let cli = parse(&[
            "ingest",
            "--wat",
            "a.wat",
            "b.wat.gz",
            "--wat",
            "c.wat",
            "--records",
            "old.jsonl",
            "--out",
            "r.jsonl",
        ])
        .unwrap();
        let Command::Ingest(args) = cli.command else {
            panic!("not ingest");
        };
        assert_eq!(args.wat.len(), 3);
        assert_eq!(args.records, vec![PathBuf::from("old.jsonl")]);
        assert_eq!(args.tranco, None);
    }

    #[test]
    fn search_takes_query_words() {
        let cli = parse(&["search", "--index", "idx", "--json", "us", "bank"]).unwrap();
        let Command::Search(args) = cli.command else {
            panic!("not search");
        };
        assert_eq!(args.query, ["us", "bank"]);
        assert_eq!(args.limit, 10);
        assert!(args.json);
        assert!(parse(&["search", "--index", "idx"]).is_err());
        assert!(parse(&["search", "--index", "idx", "--limit", "0", "x"]).is_err());
        assert!(parse(&["search", "--index", "idx", "--alpha", "1.5", "x"]).is_err());
    }

    #[test]
    fn fetch_data_release_and_url_conflict() {
        let err = parse(&[
            "fetch-data",
            "--dir",
            "data",
            "--cc-release",
            "cc-main-2025-26-nov-dec-jan",
            "--cc-ranks-url",
            "https://example.org/ranks.txt.gz",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        let cli = parse(&["fetch-data", "--dir", "data"]).unwrap();
        let Command::FetchData(args) = cli.command else {
            panic!("not fetch-data");
        };
        assert_eq!(args.wikidata_min_sitelinks, 25);
        assert!(!args.skip_tranco && !args.skip_wikidata);
    }

    #[test]
    fn eval_and_serve_defaults() {
        let cli = parse(&[
            "eval",
            "--index",
            "idx",
            "--queries",
            "q.tsv",
            "--min-top1",
            "0.9",
        ])
        .unwrap();
        let Command::Eval(args) = cli.command else {
            panic!("not eval");
        };
        assert_eq!(args.min_top1, Some(0.9));
        assert_eq!(args.limit, 10);
        assert!(parse(&["eval", "--index", "i", "--queries", "q", "--min-top1", "90"]).is_err());

        let cli = parse(&["serve", "--index", "idx"]).unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("not serve");
        };
        assert_eq!(args.bind, "127.0.0.1:8080".parse().unwrap());
    }

    #[test]
    fn crawl_defaults() {
        let cli = parse(&["crawl", "--records", "r.jsonl"]).unwrap();
        let Command::Crawl(args) = cli.command else {
            panic!("not crawl");
        };
        assert_eq!(args.top, 1000);
        assert_eq!(args.skip_crawled_within_days, 30);
        assert_eq!(args.concurrency, 16);
        assert_eq!(args.out, None);
        assert!(parse(&["crawl", "--records", "r", "--concurrency", "0"]).is_err());
    }
}
