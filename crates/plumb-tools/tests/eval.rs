//! End-to-end tests of `plumb-tools eval` over the synthetic fixtures in
//! `fixtures/`, ingested and indexed as `plumb ingest` and `plumb index` do.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use clap::Parser;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

fn args(items: &[&dyn AsRef<OsStr>]) -> Vec<OsString> {
    items.iter().map(|a| a.as_ref().to_os_string()).collect()
}

fn plumb(args: &[OsString]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_plumb-tools"))
        .args(args)
        .env("RUST_LOG", "warn")
        .env("NO_COLOR", "1")
        .output()
        .expect("running the plumb-tools binary")
}

/// Runs `plumb-tools`, failing the test with its output unless it
/// succeeds, and returns its stdout.
fn plumb_ok(args: &[OsString]) -> String {
    let output = plumb(args);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "plumb-tools {args:?} failed with {}\n--- stdout\n{stdout}\n--- stderr\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// Runs a `plumb` (node) command in this process.
fn node(items: &[&dyn AsRef<OsStr>]) {
    let all = std::iter::once(OsString::from("plumb")).chain(args(items));
    plumb_node::run(plumb_node::cli::Cli::try_parse_from(all).unwrap()).unwrap();
}

/// Ingests every fixture into `dir/records.jsonl`, indexes it into
/// `dir/index`, and returns the index path.
fn build_fixture_index(dir: &Path) -> PathBuf {
    let records = dir.join("records.jsonl");
    let index = dir.join("index");
    node(&[
        &"ingest",
        &"--tranco",
        &fixture("tranco.csv"),
        &"--cc-ranks",
        &fixture("cc-domain-ranks.txt"),
        &"--wat",
        &fixture("sample.wat"),
        &"--wikidata",
        &fixture("wikidata-official-sites.tsv"),
        &"--out",
        &records,
    ]);
    node(&[&"index", &"--records", &records, &"--index", &index]);
    index
}

#[test]
fn eval_scores_the_fixtures() {
    let tmp = tempfile::tempdir().unwrap();
    let index = build_fixture_index(tmp.path());
    let eval = plumb_ok(&args(&[
        &"eval",
        &"--index",
        &index,
        &"--queries",
        &fixture("brand_queries.tsv"),
        &"--min-top1",
        &"0.9",
    ]));
    assert!(eval.contains("top-1"), "{eval}");
    assert!(eval.contains("MRR@10"), "{eval}");
}

#[test]
fn eval_scores_the_same_with_the_page_index_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let index = build_fixture_index(tmp.path());
    let pages = tmp.path().join("wikipedia-en.tsv");
    std::fs::write(
        &pages,
        "views\ttitle\tdescription\titem\tsite\taliases\n\
         9000\tU.S. Bancorp\tAmerican bank holding company\tQ1\tusbank.com\tUS Bank\n\
         8000\tMarie Curie\tPolish-French physicist and chemist\tQ7186\t\t\n\
         7000\tChase Bank\tAmerican bank\tQ2\tchase.com\t\n",
    )
    .unwrap();
    let queries = tmp.path().join("queries.tsv");
    std::fs::write(
        &queries,
        "us bank\tusbank.com\n\
         marie curie\thttps://en.wikipedia.org/wiki/Marie_Curie\n\
         chase\tchase.com\n",
    )
    .unwrap();
    let cache = tmp.path().join("cache");
    let eval = |cached: bool| {
        let mut items: Vec<&dyn AsRef<OsStr>> = vec![
            &"eval",
            &"--index",
            &index,
            &"--queries",
            &queries,
            &"--pages",
            &pages,
            &"--show",
            &"3",
        ];
        if cached {
            items.extend([&"--pages-cache" as &dyn AsRef<OsStr>, &cache]);
        }
        plumb_ok(&args(&items))
    };
    let fresh = eval(false);
    assert!(fresh.contains("top-1"), "{fresh}");
    // The first cached run builds the index, the second reuses it.
    assert_eq!(eval(true), fresh);
    assert_eq!(eval(true), fresh);
    let kept: Vec<_> = std::fs::read_dir(&cache)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.len() == 64)
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");

    // A changed page set file builds another index.
    std::fs::write(
        &pages,
        std::fs::read_to_string(&pages).unwrap() + "100\tChase\tA pursuit\tQ3\t\t\n",
    )
    .unwrap();
    eval(true);
    let kept = std::fs::read_dir(&cache)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_name().len() == 64)
        .count();
    assert_eq!(kept, 2);
}

#[test]
fn eval_fails_below_min_top1_and_reports_misses() {
    let tmp = tempfile::tempdir().unwrap();
    let index = build_fixture_index(tmp.path());
    let queries = tmp.path().join("queries.tsv");
    std::fs::write(
        &queries,
        "# one right, one wrong on purpose\nus bank\tusbank.com\nus bank\tchase.com\n",
    )
    .unwrap();
    let eval_args = |min: &str| {
        args(&[
            &"eval",
            &"--index",
            &index,
            &"--queries",
            &queries,
            &"--min-top1",
            &min,
        ])
    };

    let output = plumb(&eval_args("0.9"));
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("miss: \"us bank\" (line 3) expected chase.com"),
        "{stdout}"
    );
    assert!(stdout.contains("first was usbank.com"), "{stdout}");
    assert!(stdout.contains("top-1    50.0%  (1/2)"), "{stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("below --min-top1"), "{stderr}");

    plumb_ok(&eval_args("0.5"));
}
