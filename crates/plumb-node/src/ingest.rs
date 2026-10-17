//! `plumb ingest`: folds seed data and earlier records files into one
//! records file, each site its stored item ([`plumb_core::site`]).
//!
//! Two rules keep a large or repeated ingest safe:
//!
//! - The Common Crawl ranks file lists over 100M domains, best first, and each
//!   million rows read takes about 0.9 GB of memory until the records are
//!   written. Unless `--limit-per-source` says otherwise, only the top rows
//!   are read: [`CC_ROWS_PER_TOP_RECORD`] for each record `--top` keeps, or
//!   [`DEFAULT_CC_RANK_ROWS`] without `--top`.
//! - Seed signals in `--records` files give way to seed files of the same kind
//!   (see [`FreshSeeds`]), so a rank or an official-site mark lasts only as
//!   long as the source that gave it.
//!
//! `--records` files are read with the journal a crawl may have left next to
//! them ([`crate::records`]), and `--out` is replaced whole, its own journal
//! deleted: that journal holds changes to the file being replaced, and
//! replaying them onto the new one would be wrong.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use plumb_core::site;
use plumb_core::sites::SiteList;
use plumb_format::Item;
use plumb_ingest::{
    attach_facts, attach_intros, load_cc_domain_ranks, load_intros, load_site_facts, load_tranco,
    load_wikidata_official_sites, parse_wat, Builder, WatExtract, WatStats,
};
use tracing::{info, warn};

use crate::cli::IngestArgs;
use crate::records::{load_sites, replace_sites};

/// Common Crawl rank rows read for each record `--top` keeps, when
/// `--limit-per-source` is not given. The file is sorted by harmonic
/// centrality, which is only one input to the link score: a site a little
/// further down can still make the cut through its PageRank position, a
/// Wikidata listing or links seen in WAT files, so the cut leaves room for
/// as many rows again. The help of `--limit-per-source` and the README call
/// this "twice --top".
const CC_ROWS_PER_TOP_RECORD: usize = 2;

/// Common Crawl rank rows read when neither `--limit-per-source` nor `--top`
/// is given: as many as the Tranco list has.
const DEFAULT_CC_RANK_ROWS: usize = 1_000_000;

pub fn run(args: IngestArgs) -> Result<()> {
    check_inputs_exist(&args)?;
    let limit = args.limit_per_source;
    let fresh = FreshSeeds::of(&args);
    let mut builder = Builder::new();

    if let Some(path) = &args.tranco {
        let entries = load_tranco(path, limit)
            .with_context(|| format!("loading the Tranco list {}", path.display()))?;
        builder.add_tranco(&entries);
        println!(
            "tranco    {:>9} ranked domains  ({})",
            entries.len(),
            path.display()
        );
    }

    if let Some(path) = &args.cc_ranks {
        let (rows, implicit) = cc_rank_rows(limit, args.top);
        let ranks = load_cc_domain_ranks(path, Some(rows))
            .with_context(|| format!("loading Common Crawl ranks {}", path.display()))?;
        builder.add_cc_ranks(&ranks);
        println!(
            "cc-ranks  {:>9} ranked domains  ({})",
            ranks.len(),
            path.display()
        );
        if let Some(why) = implicit.filter(|_| ranks.len() >= rows) {
            println!(
                "          read only the top {rows} rows ({why}); --limit-per-source N reads more, \
                 at about 0.9 GB of memory per million rows"
            );
        }
    }

    if !args.wat.is_empty() {
        let mut extract = WatExtract::new();
        let mut stats = WatStats::default();
        // One corrupt or truncated file (common in a large download) does
        // not throw away the others: it is skipped, with what it gave before
        // failing kept, and counted.
        let mut failed_files = 0;
        for path in &args.wat {
            let file_stats = match parse_wat(path, &mut extract) {
                Ok(file_stats) => file_stats,
                Err(err) => {
                    warn!("skipping WAT file {}: {err:#}", path.display());
                    failed_files += 1;
                    continue;
                }
            };
            info!(
                "{}: {} records, {} pages, {} homepages, {} links",
                path.display(),
                file_stats.records,
                file_stats.responses,
                file_stats.homepages,
                file_stats.links
            );
            stats.add(&file_stats);
        }
        if failed_files == args.wat.len() {
            bail!("none of the {failed_files} WAT files could be read");
        }
        builder.add_wat(&extract);
        println!(
            "wat       {:>9} homepages       ({} files: {} pages, {} cross-site links to {} domains, {} bad records, {} unreadable files)",
            extract.homepages.len(),
            args.wat.len(),
            stats.responses,
            stats.links,
            extract.linking_domains.len(),
            stats.bad_records,
            failed_files
        );
    }

    if let Some(path) = &args.wikidata {
        // Not cut to --limit-per-source: the file is not ranked, and the shared-host
        // check in add_official_sites needs every item that claims a domain.
        let mut sites = load_wikidata_official_sites(path)
            .with_context(|| format!("loading Wikidata sites {}", path.display()))?;
        if let Some(kinds_path) = &args.wikidata_kinds {
            let by_kind = load_wikidata_official_sites(kinds_path)
                .with_context(|| format!("loading Wikidata sites {}", kinds_path.display()))?;
            println!(
                "kinds     {:>9} official sites  ({})",
                by_kind.len(),
                kinds_path.display()
            );
            sites.extend(by_kind);
        }
        if let Some(facts_path) = &args.wikidata_facts {
            let facts = load_site_facts(facts_path)
                .with_context(|| format!("loading Wikidata facts {}", facts_path.display()))?;
            attach_facts(&mut sites, &facts);
            println!(
                "facts     {:>9} items           ({})",
                facts.len(),
                facts_path.display()
            );
        }
        if let Some(intros_path) = &args.wikipedia_intros {
            let intros = load_intros(intros_path)
                .with_context(|| format!("loading Wikipedia intros {}", intros_path.display()))?;
            attach_intros(&mut sites, &intros);
            println!(
                "intros    {:>9} items           ({})",
                intros.len(),
                intros_path.display()
            );
        }
        builder.add_official_sites(&sites);
        println!(
            "wikidata  {:>9} official sites  ({})",
            sites.len(),
            path.display()
        );
    }

    if !args.records.is_empty() {
        if let Some(dropped) = fresh.describe() {
            info!("the seed files given replace the {dropped} in the records files");
        }
    }
    for path in &args.records {
        let mut sites = load_sites(path)
            .with_context(|| format!("loading records {}", path.display()))?
            .into_sorted();
        if let Some(n) = limit {
            sites.truncate(n);
        }
        println!(
            "records   {:>9} records         ({})",
            sites.len(),
            path.display()
        );
        builder.add_sites(sites.into_iter().map(|mut site| {
            fresh.strip(&mut site);
            site
        }));
    }

    let sites = builder.finish(args.top);
    let written = replace_sites(&args.out, &sites)?;
    let summary = RecordSummary::of(&sites);
    println!(
        "wrote {written} records to {} ({} with a homepage title, {} with link text, {} official sites)",
        args.out.display(),
        summary.with_title,
        summary.with_link_text,
        summary.official
    );
    Ok(())
}

/// How many Common Crawl rank rows to read: `--limit-per-source` when given,
/// else [`CC_ROWS_PER_TOP_RECORD`] for each record `--top` keeps, else
/// [`DEFAULT_CC_RANK_ROWS`]. The second value says where an implicit bound
/// came from, for the summary.
fn cc_rank_rows(limit_per_source: Option<usize>, top: Option<usize>) -> (usize, Option<String>) {
    match (limit_per_source, top) {
        (Some(limit), _) => (limit, None),
        (None, Some(top)) => (
            top.saturating_mul(CC_ROWS_PER_TOP_RECORD),
            Some(format!("--top {top} times {CC_ROWS_PER_TOP_RECORD}")),
        ),
        (None, None) => (
            DEFAULT_CC_RANK_ROWS,
            Some("the default without --top".to_string()),
        ),
    }
}

/// The seed sources an ingest reads afresh. Their signals are dropped from
/// `--records` files before merging. Merging keeps the best rank and ORs
/// `official_site`, so otherwise a domain that has since lost its rank or its
/// Wikidata listing (one that expired and was registered again by someone
/// else, say) would keep both for as long as its record is carried forward.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FreshSeeds {
    tranco: bool,
    cc_ranks: bool,
    wikidata: bool,
}

impl FreshSeeds {
    fn of(args: &IngestArgs) -> Self {
        FreshSeeds {
            tranco: args.tranco.is_some(),
            cc_ranks: args.cc_ranks.is_some(),
            wikidata: args.wikidata.is_some(),
        }
    }

    /// Drops from `site` the signals of the seed kinds read afresh:
    ///
    /// - Tranco: `tranco_rank`.
    /// - Common Crawl ranks: `harmonic_rank` and `pagerank_rank`.
    /// - Wikidata: `official_site`, and every name of a site that had it.
    ///   Names do not say where they came from, but Wikidata labels only go
    ///   to official sites, so the names of other sites (`og:site_name`
    ///   from WAT files and crawls) are kept. An official site's own
    ///   `og:site_name` comes back on its next crawl, and its current
    ///   Wikidata label straight away if Wikidata still lists it.
    ///
    /// Page fields, link texts, `linking_domains` and crawl times stay.
    fn strip(self, site: &mut Item) {
        let mut signals = site::signals(site);
        if self.tranco {
            signals.tranco_rank = None;
        }
        if self.cc_ranks {
            signals.harmonic_rank = None;
            signals.pagerank_rank = None;
        }
        if self.wikidata && signals.official_site {
            signals.official_site = false;
            signals.sitelinks = 0;
            site.names.clear();
        }
        site::set_signals(site, &signals);
        if self.wikidata {
            // These come only from Wikidata.
            site::set_country(site, None);
            site::set_kinds(site, Vec::new());
            site::set_wikidata_about(site, None);
            site::set_intro(site, None);
        }
    }

    /// What [`FreshSeeds::strip`] drops, for the log; `None` when nothing.
    fn describe(self) -> Option<String> {
        let dropped: Vec<&str> = [
            (self.tranco, "Tranco ranks"),
            (self.cc_ranks, "Common Crawl ranks"),
            (
                self.wikidata,
                "official-site marks (with those sites' aliases)",
            ),
        ]
        .into_iter()
        .filter_map(|(fresh, what)| fresh.then_some(what))
        .collect();
        match dropped.as_slice() {
            [] => None,
            [one] => Some(one.to_string()),
            [rest @ .., last] => Some(format!("{} and {last}", rest.join(", "))),
        }
    }
}

/// Fails fast on a mistyped path, before minutes go into reading the others.
fn check_inputs_exist(args: &IngestArgs) -> Result<()> {
    let inputs: Vec<&PathBuf> = args
        .tranco
        .iter()
        .chain(&args.cc_ranks)
        .chain(&args.wat)
        .chain(&args.wikidata)
        .chain(&args.wikidata_facts)
        .chain(&args.wikipedia_intros)
        .chain(&args.wikidata_kinds)
        .chain(&args.records)
        .collect();
    let missing: Vec<String> = inputs
        .iter()
        .filter(|p| !Path::is_file(p))
        .map(|p| p.display().to_string())
        .collect();
    if !missing.is_empty() {
        bail!("input file not found: {}", missing.join(", "));
    }
    Ok(())
}

/// Counts that show at a glance whether an ingest produced something useful.
#[derive(Debug, Default, PartialEq, Eq)]
struct RecordSummary {
    with_title: usize,
    with_link_text: usize,
    official: usize,
}

impl RecordSummary {
    fn of(sites: &SiteList) -> Self {
        let mut summary = RecordSummary::default();
        for held in sites {
            summary.with_title += usize::from(held.title.is_some());
            summary.with_link_text += usize::from(!site::link_texts(&held).is_empty());
            summary.official += usize::from(site::signals(&held).official_site);
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use plumb_core::site::CrawlMarks;

    use super::*;
    use crate::records::{journal_path, Change, RecordStore};
    use crate::test_lines::{read_sites, write_old_lines};

    /// Arguments that read nothing and write to `out`; tests fill in the rest.
    fn writing_to(out: PathBuf) -> IngestArgs {
        IngestArgs {
            tranco: None,
            cc_ranks: None,
            wat: Vec::new(),
            wikidata: None,
            wikidata_facts: None,
            wikipedia_intros: None,
            wikidata_kinds: None,
            records: Vec::new(),
            limit_per_source: None,
            top: None,
            out,
        }
    }

    fn args_with(paths: &[PathBuf]) -> IngestArgs {
        IngestArgs {
            wat: paths.to_vec(),
            ..writing_to(PathBuf::from("out.jsonl"))
        }
    }

    /// Writes `contents` to `dir/name` and returns the path.
    fn file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// A Common Crawl domain ranks file with one row per
    /// `(harmonic position, PageRank position, domain)`.
    fn cc_ranks(rows: &[(u64, u64, &str)]) -> String {
        let mut text =
            "#harmonicc_pos\t#harmonicc_val\t#pr_pos\t#pr_val\t#host_rev\t#n_hosts\n".to_string();
        for (harmonic, pagerank, domain) in rows {
            let reversed: Vec<&str> = domain.rsplit('.').collect();
            text += &format!(
                "{harmonic}\t1.0\t{pagerank}\t0.001\t{}\t1\n",
                reversed.join(".")
            );
        }
        text
    }

    /// Runs the ingest and reads back what it wrote.
    fn ingest(args: IngestArgs) -> Vec<Item> {
        let out = args.out.clone();
        run(args).unwrap();
        read_sites(&out)
    }

    fn find<'a>(sites: &'a [Item], domain: &str) -> &'a Item {
        sites
            .iter()
            .find(|site| site.id == domain)
            .unwrap_or_else(|| panic!("no {domain} site"))
    }

    fn domains(sites: &[Item]) -> Vec<&str> {
        sites.iter().map(|site| site.id.as_str()).collect()
    }

    #[test]
    fn missing_inputs_are_reported_together() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("present.wat");
        std::fs::write(&present, "").unwrap();
        assert!(check_inputs_exist(&args_with(std::slice::from_ref(&present))).is_ok());

        let gone_a = dir.path().join("a.wat");
        let gone_b = dir.path().join("b.wat");
        let err = check_inputs_exist(&args_with(&[present, gone_a, gone_b])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("a.wat") && msg.contains("b.wat"), "{msg}");
    }

    #[test]
    fn an_unreadable_wat_file_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let empty = file(dir.path(), "empty.wat", "");
        let broken = file(dir.path(), "broken.wat", "not a WARC file\r\n\r\n");
        let records = ingest(IngestArgs {
            wat: vec![broken.clone(), empty],
            ..writing_to(dir.path().join("out.jsonl"))
        });
        assert!(records.is_empty());
        // With nothing readable, the ingest fails.
        let err = run(IngestArgs {
            wat: vec![broken],
            ..writing_to(dir.path().join("none.jsonl"))
        })
        .unwrap_err();
        assert!(err.to_string().contains("none of the 1 WAT files"), "{err}");
    }

    #[test]
    fn summary_counts() {
        let mut a = site::new("a.com");
        a.title = Some("A".into());
        let mut signals = site::signals(&a);
        signals.official_site = true;
        site::set_signals(&mut a, &signals);
        let mut b = site::new("b.com");
        site::add_link_text(&mut b, "Bee", "a.com");
        let sites: SiteList = [a, b, site::new("c.com")].into_iter().collect();
        let summary = RecordSummary::of(&sites);
        assert_eq!(
            summary,
            RecordSummary {
                with_title: 1,
                with_link_text: 1,
                official: 1
            }
        );
    }

    #[test]
    fn common_crawl_rows_are_bounded() {
        assert_eq!(cc_rank_rows(Some(50), Some(10)), (50, None));
        assert_eq!(cc_rank_rows(Some(50), None), (50, None));
        let (rows, why) = cc_rank_rows(None, Some(10));
        assert_eq!(rows, 10 * CC_ROWS_PER_TOP_RECORD);
        assert_eq!(why.as_deref(), Some("--top 10 times 2"));
        assert_eq!(cc_rank_rows(None, Some(usize::MAX)).0, usize::MAX);
        let (rows, why) = cc_rank_rows(None, None);
        assert_eq!(rows, DEFAULT_CC_RANK_ROWS);
        assert_eq!(why.as_deref(), Some("the default without --top"));
    }

    #[test]
    fn common_crawl_ranks_are_read_down_to_twice_top() {
        let dir = tempfile::tempdir().unwrap();
        // Sorted by harmonic centrality like the real file. The fifth row has
        // the best PageRank position, so it makes the top two if it is read.
        let rows = [
            (1, 2, "one.com"),
            (2, 3, "two.com"),
            (3, 4, "three.com"),
            (4, 5, "four.com"),
            (5, 1, "five.com"),
        ];
        let ranks = file(dir.path(), "ranks.txt", &cc_ranks(&rows));
        let cc_args = |out: &str| IngestArgs {
            cc_ranks: Some(ranks.clone()),
            ..writing_to(dir.path().join(out))
        };

        // --top 2 reads four rows.
        let top2 = ingest(IngestArgs {
            top: Some(2),
            ..cc_args("top2.jsonl")
        });
        assert_eq!(domains(&top2), ["one.com", "two.com"]);

        // --limit-per-source reads as many as it says.
        let limited = ingest(IngestArgs {
            top: Some(2),
            limit_per_source: Some(5),
            ..cc_args("limited.jsonl")
        });
        assert_eq!(domains(&limited), ["five.com", "one.com"]);

        // Without either, far more rows than this file has.
        assert_eq!(ingest(cc_args("all.jsonl")).len(), rows.len());
    }

    /// A site with every kind of seed signal, as a 2025 ingest and a crawl
    /// would leave it.
    fn stale_official_site() -> Item {
        let mut record = site::new("olddomain.com");
        let mut signals = site::signals(&record);
        signals.tranco_rank = Some(500);
        signals.harmonic_rank = Some(700);
        signals.pagerank_rank = Some(650);
        signals.official_site = true;
        signals.linking_domains = 3;
        site::set_signals(&mut record, &signals);
        site::add_alias(&mut record, "Acme Corporation");
        record.title = Some("Acme Corporation".into());
        site::add_link_text(&mut record, "acme", "news.example");
        site::set_crawled(&mut record, Some(1_700_000_000));
        record
    }

    #[test]
    fn only_the_seed_kinds_read_afresh_are_dropped() {
        let stale = stale_official_site();
        let stripped = |fresh: FreshSeeds| {
            let mut site = stale.clone();
            fresh.strip(&mut site);
            site
        };

        assert_eq!(stripped(FreshSeeds::default()), stale);

        let r = stripped(FreshSeeds {
            tranco: true,
            ..FreshSeeds::default()
        });
        assert_eq!(site::signals(&r).tranco_rank, None);
        assert_eq!(site::signals(&r).harmonic_rank, Some(700));
        assert!(site::signals(&r).official_site);

        let r = stripped(FreshSeeds {
            cc_ranks: true,
            ..FreshSeeds::default()
        });
        let signals = site::signals(&r);
        assert_eq!((signals.harmonic_rank, signals.pagerank_rank), (None, None));
        assert_eq!(signals.tranco_rank, Some(500));

        let r = stripped(FreshSeeds {
            wikidata: true,
            ..FreshSeeds::default()
        });
        assert!(!site::signals(&r).official_site);
        assert!(r.names.is_empty());
        assert_eq!(site::signals(&r).tranco_rank, Some(500));

        // Whatever is read afresh, page fields, link texts, linking domains
        // and crawl times stay.
        let all = FreshSeeds {
            tranco: true,
            cc_ranks: true,
            wikidata: true,
        };
        let r = stripped(all);
        assert_eq!(r.title, stale.title);
        assert_eq!(site::link_texts(&r), site::link_texts(&stale));
        assert_eq!(site::signals(&r).linking_domains, 3);
        assert_eq!(site::crawled(&r), site::crawled(&stale));

        // A site Wikidata never listed keeps its aliases: they can only be
        // og:site_name from WAT files or crawls.
        let mut fans = site::new("fans.net");
        fans.names = vec!["Acme Fans".into()];
        all.strip(&mut fans);
        assert_eq!(fans.names, ["Acme Fans"]);
    }

    #[test]
    fn wikidata_read_afresh_drops_all_wikidata_says_and_official_sites_names() {
        let mut full = stale_official_site();
        let mut signals = site::signals(&full);
        signals.sitelinks = 40;
        signals.fact_checks = 2;
        signals.fact_agrees = 1;
        site::set_signals(&mut full, &signals);
        site::set_country(&mut full, Some("US".into()));
        site::set_kinds(&mut full, vec!["company".into(), "anvil maker".into()]);
        site::set_wikidata_about(&mut full, Some("maker of anvils".into()));
        site::set_intro(&mut full, Some("Acme is a company.".into()));
        full.about = Some("Anvils since 1949".into());
        let mut unlisted = full.clone();
        unlisted.id = "unlisted.com".into();
        let mut signals = site::signals(&unlisted);
        signals.official_site = false;
        site::set_signals(&mut unlisted, &signals);
        let wikidata = FreshSeeds {
            wikidata: true,
            ..FreshSeeds::default()
        };

        // What Wikidata said goes, and an official site's names and
        // sitelinks with it.
        let mut official = full.clone();
        wikidata.strip(&mut official);
        let signals = site::signals(&official);
        assert!(!signals.official_site);
        assert_eq!(signals.sitelinks, 0);
        assert!(official.names.is_empty());
        assert_eq!(site::country(&official), None);
        assert!(site::kinds(&official).is_empty());
        assert_eq!(site::wikidata_about(&official), None);
        assert_eq!(site::intro(&official), None);
        // The rest stays: ranks, fact checks, the page's own description.
        assert_eq!(
            (
                signals.tranco_rank,
                signals.harmonic_rank,
                signals.pagerank_rank
            ),
            (Some(500), Some(700), Some(650))
        );
        assert_eq!((signals.fact_checks, signals.fact_agrees), (2, 1));
        assert_eq!(official.about.as_deref(), Some("Anvils since 1949"));
        assert_eq!(official.title, full.title);

        // A site that was no official site keeps its names and sitelinks.
        let mut other = unlisted.clone();
        wikidata.strip(&mut other);
        assert_eq!(other.names, ["Acme Corporation"]);
        assert_eq!(site::signals(&other).sitelinks, 40);
        assert_eq!(site::country(&other), None);
        assert!(site::kinds(&other).is_empty());
        assert_eq!(site::wikidata_about(&other), None);
        assert_eq!(site::intro(&other), None);

        // Ranks read afresh leave what Wikidata said.
        let mut ranked = full.clone();
        let ranks = FreshSeeds {
            tranco: true,
            cc_ranks: true,
            wikidata: false,
        };
        ranks.strip(&mut ranked);
        let mut expected = full.clone();
        let mut signals = site::signals(&expected);
        signals.tranco_rank = None;
        signals.harmonic_rank = None;
        signals.pagerank_rank = None;
        site::set_signals(&mut expected, &signals);
        assert_eq!(ranked, expected);

        // An empty site is left as it is whatever is read afresh.
        for bits in 0..8 {
            let fresh = FreshSeeds {
                tranco: bits & 1 != 0,
                cc_ranks: bits & 2 != 0,
                wikidata: bits & 4 != 0,
            };
            let mut empty = site::new("empty.com");
            fresh.strip(&mut empty);
            assert_eq!(empty, site::new("empty.com"), "{fresh:?}");
        }
    }

    #[test]
    fn describes_what_the_seed_files_replace() {
        let fresh = |tranco, cc_ranks, wikidata| FreshSeeds {
            tranco,
            cc_ranks,
            wikidata,
        };
        assert_eq!(fresh(false, false, false).describe(), None);
        assert_eq!(
            fresh(true, false, false).describe().as_deref(),
            Some("Tranco ranks")
        );
        assert_eq!(
            fresh(true, true, false).describe().as_deref(),
            Some("Tranco ranks and Common Crawl ranks")
        );
        assert_eq!(
            fresh(true, true, true).describe().as_deref(),
            Some(
                "Tranco ranks, Common Crawl ranks and official-site marks (with those sites' aliases)"
            )
        );
    }

    #[test]
    fn a_refresh_drops_trust_from_old_seed_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name);
        let wikidata = |name: &str, website: &str| {
            let tsv = format!("item\tlabel\twebsite\nQ1\tAcme Corporation\t{website}\n");
            Some(file(dir.path(), name, &tsv))
        };

        // 2025: olddomain.com is Acme Corporation's official site, at #500.
        let mut records = ingest(IngestArgs {
            tranco: Some(file(
                dir.path(),
                "tranco-2025.csv",
                "500,olddomain.com\n800,fans.net\n",
            )),
            cc_ranks: Some(file(
                dir.path(),
                "cc-2025.txt",
                &cc_ranks(&[(700, 650, "olddomain.com")]),
            )),
            wikidata: wikidata("wikidata-2025.tsv", "https://www.olddomain.com/"),
            ..writing_to(path("records.jsonl"))
        });
        let old = find(&records, "olddomain.com");
        assert!(site::signals(old).official_site);
        assert_eq!(old.names, ["Acme Corporation"]);

        // Crawls add page titles, link text and og:site_name.
        for record in &mut records {
            record.title = Some(format!("{} home", record.id));
            site::add_link_text(record, "acme", "news.example");
            let mut signals = site::signals(record);
            signals.linking_domains = 3;
            site::set_signals(record, &signals);
            site::set_crawled(record, Some(1_700_000_000));
            let marks = CrawlMarks {
                tried: Some(1_700_000_100),
                ..CrawlMarks::default()
            };
            site::set_crawl_marks(record, marks);
            let site_name = if record.id == "fans.net" {
                "Acme Fans"
            } else {
                "Acme"
            };
            site::add_alias(record, site_name);
        }
        write_old_lines(&path("records.jsonl"), &records);

        // 2026: olddomain.com has expired. It is down to #900000, out of the
        // Common Crawl ranks, and Wikidata points at acme.com instead.
        let refreshed = ingest(IngestArgs {
            tranco: Some(file(
                dir.path(),
                "tranco-2026.csv",
                "1000,acme.com\n900000,olddomain.com\n",
            )),
            cc_ranks: Some(file(
                dir.path(),
                "cc-2026.txt",
                &cc_ranks(&[(5000, 4000, "acme.com")]),
            )),
            wikidata: wikidata("wikidata-2026.tsv", "https://acme.com/"),
            records: vec![path("records.jsonl")],
            ..writing_to(path("records.jsonl"))
        });

        let old = find(&refreshed, "olddomain.com");
        assert_eq!(site::signals(old).tranco_rank, Some(900_000));
        assert_eq!(
            (
                site::signals(old).harmonic_rank,
                site::signals(old).pagerank_rank
            ),
            (None, None)
        );
        assert!(!site::signals(old).official_site);
        assert!(old.names.is_empty(), "{:?}", old.names);
        // What the crawls found stays.
        assert_eq!(old.title.as_deref(), Some("olddomain.com home"));
        assert!(site::link_texts(old).iter().any(|lt| lt.text == "acme"));
        assert_eq!(site::signals(old).linking_domains, 3);
        assert_eq!(
            (site::crawled(old), site::crawl_marks(old).tried),
            (Some(1_700_000_000), Some(1_700_000_100))
        );

        let acme = find(&refreshed, "acme.com");
        assert!(site::signals(acme).official_site);
        assert_eq!(acme.names, ["Acme Corporation"]);
        assert!(site::link_score(acme) > site::link_score(old));
        assert_eq!(refreshed[0].id, "acme.com");

        // A site Wikidata never listed keeps its og:site_name, but its rank
        // left with the 2025 list.
        let fans = find(&refreshed, "fans.net");
        assert_eq!(fans.names, ["Acme Fans"]);
        assert_eq!(site::signals(fans).tranco_rank, None);
    }

    #[test]
    fn crawl_journals_are_read_with_their_records_and_dropped_at_out() {
        let dir = tempfile::tempdir().unwrap();
        let titled = |domain: &str, title: &str| {
            let mut record = site::new(domain);
            record.title = Some(title.into());
            record
        };
        // A records file whose crawl was cut short, its results in a journal.
        let records = dir.path().join("records.jsonl");
        let mut known = site::new("known.com");
        let mut signals = site::signals(&known);
        signals.tranco_rank = Some(10);
        site::set_signals(&mut known, &signals);
        write_old_lines(&records, &[site::new("unranked.com"), known]);
        RecordStore::open(&records)
            .save(&[
                Change::Merge {
                    site: titled("found.com", "Found"),
                },
                Change::Mark {
                    domain: "known.com".into(),
                    attempted_at: Some(5),
                    failures: 1,
                },
            ])
            .unwrap();
        // A file to be replaced, with a journal of its own.
        let out = dir.path().join("out.jsonl");
        write_old_lines(&out, &[site::new("old.com")]);
        RecordStore::open(&out)
            .save(&[Change::Merge {
                site: titled("stale.com", "Stale"),
            }])
            .unwrap();

        let written = ingest(IngestArgs {
            records: vec![records.clone()],
            ..writing_to(out.clone())
        });
        assert_eq!(written.len(), 3);
        assert_eq!(find(&written, "found.com").title.as_deref(), Some("Found"));
        let known = site::crawl_marks(find(&written, "known.com"));
        assert_eq!((known.tried, known.failures), (Some(5), 1));
        // The old file's journal is gone rather than replayed onto the new one.
        assert!(!journal_path(&out).exists());
        let reread = load_sites(&out).unwrap();
        assert_eq!(reread.len(), 3);
        assert!(!reread.contains("stale.com"));
        // Only --out is replaced: the input keeps its journal.
        assert!(journal_path(&records).exists());

        // --limit-per-source keeps the best records, whatever the file order.
        let best = ingest(IngestArgs {
            records: vec![records.clone()],
            limit_per_source: Some(1),
            ..writing_to(dir.path().join("best.jsonl"))
        });
        assert_eq!(domains(&best), ["known.com"]);

        // Refreshing a records file in place folds its journal in.
        ingest(IngestArgs {
            records: vec![records.clone()],
            ..writing_to(records.clone())
        });
        assert!(!journal_path(&records).exists());
        let file = read_sites(&records);
        assert_eq!(find(&file, "found.com").title.as_deref(), Some("Found"));
    }
}
