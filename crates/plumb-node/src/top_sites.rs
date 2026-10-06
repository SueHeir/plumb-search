//! `plumb top-sites`: how much of a records file the best sites take, and
//! a copy cut down to them, to measure what each size of node can search.
//!
//! Sites are ranked by link score, as a node fills and trims (see
//! `crate::node::trim`). Each falls in one of three kinds: named (a
//! homepage title, or a name from Wikidata or an About page), link-only
//! (no name, only the words other sites link to it with, or nothing at
//! all) and gone (cut down by `--drop-dead-sites`).

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};
use plumb_core::SiteRecord;

use crate::cli::TopSitesArgs;
use crate::web::group_thousands;

/// The site counts the report gives the size of the best sites at.
const MARKS: [usize; 7] = [
    100_000, 250_000, 500_000, 1_000_000, 2_000_000, 5_000_000, 10_000_000,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Named,
    LinkOnly,
    Gone,
}

impl Kind {
    fn of(record: &SiteRecord) -> Kind {
        if record.gone_at.is_some() {
            Kind::Gone
        } else if record
            .title
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
            || !record.aliases.is_empty()
        {
            Kind::Named
        } else {
            Kind::LinkOnly
        }
    }
}

/// One site as the first pass saw it.
struct Seen {
    score: f32,
    domain: Box<str>,
    bytes: u64,
    kind: Kind,
}

/// Bytes and sites of one kind.
#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    sites: u64,
    bytes: u64,
}

impl Tally {
    fn add(&mut self, bytes: u64) {
        self.sites += 1;
        self.bytes += bytes;
    }
}

pub fn run(args: &TopSitesArgs) -> Result<()> {
    let dir = match args.records.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    // A journal next to the file is folded into a copy, leaving the file
    // (and any node using it) alone.
    let copy = tempfile::Builder::new()
        .prefix(".plumb-top-sites-")
        .suffix(".jsonl")
        .tempfile_in(dir)
        .with_context(|| format!("making a file in {}", dir.display()))?
        .into_temp_path();
    let read_from = match crate::outline::outline_copy(&args.records, &copy)
        .with_context(|| format!("reading records {}", args.records.display()))?
    {
        Some((path, _)) => path.to_path_buf(),
        None => args.records.clone(),
    };
    let mut seen = Vec::new();
    let mut failed = None;
    crate::outline::for_each_record(&read_from, |record| match serde_json::to_vec(&record) {
        Ok(line) => seen.push(Seen {
            score: record.link_score(),
            domain: record.domain.as_str().into(),
            bytes: line.len() as u64 + 1,
            kind: Kind::of(&record),
        }),
        Err(err) => {
            failed.get_or_insert(err);
        }
    })?;
    if let Some(err) = failed {
        return Err(err).context("encoding a record");
    }
    seen.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.domain.cmp(&b.domain))
    });
    print!("{}", report(&seen));

    let Some(out) = &args.out else {
        return Ok(());
    };
    let kept: HashSet<&str> = seen
        .iter()
        .filter(|site| keeps(site, args.named_only))
        .take(args.top.unwrap_or(usize::MAX))
        .map(|site| &*site.domain)
        .collect();
    let file = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut writer = BufWriter::new(file);
    let mut written = Tally::default();
    let mut failed = None;
    crate::outline::for_each_record(&read_from, |record| {
        if failed.is_some() || !kept.contains(record.domain.as_str()) {
            return;
        }
        let line = serde_json::to_vec(&record)
            .map_err(anyhow::Error::from)
            .and_then(|line| {
                writer.write_all(&line)?;
                writer.write_all(b"\n")?;
                Ok(line.len() as u64 + 1)
            });
        match line {
            Ok(bytes) => written.add(bytes),
            Err(err) => failed = Some(err),
        }
    })?;
    if let Some(err) = failed {
        return Err(err).with_context(|| format!("writing {}", out.display()));
    }
    writer
        .flush()
        .with_context(|| format!("writing {}", out.display()))?;
    println!(
        "wrote {} sites ({}) to {}",
        group_thousands(written.sites),
        megabytes(written.bytes),
        out.display()
    );
    Ok(())
}

/// Whether a cut keeps `site`: gone sites never, link-only ones unless
/// only named sites are kept.
fn keeps(site: &Seen, named_only: bool) -> bool {
    match site.kind {
        Kind::Named => true,
        Kind::LinkOnly => !named_only,
        Kind::Gone => false,
    }
}

/// The sites and bytes of each kind, then how much the best sites take at
/// each of [`MARKS`], all of them and only the named ones. `seen` is best
/// first.
fn report(seen: &[Seen]) -> String {
    let mut out = String::new();
    let mut kinds = [Tally::default(); 3];
    for site in seen {
        kinds[site.kind as usize].add(site.bytes);
    }
    let total: u64 = kinds.iter().map(|k| k.bytes).sum();
    out.push_str(&format!(
        "{} sites, {} of records\n",
        group_thousands(seen.len() as u64),
        megabytes(total)
    ));
    for (name, tally) in ["named", "link-only", "gone"].iter().zip(kinds) {
        out.push_str(&format!(
            "  {name:<10} {:>12} sites  {:>10}\n",
            group_thousands(tally.sites),
            megabytes(tally.bytes)
        ));
    }
    out.push_str("best sites   all kept               named only\n");
    let (mut all, mut named) = (Tally::default(), Tally::default());
    let row = |label: String, all: Tally, named: Tally| {
        format!(
            "  {label:>10}  {:>10} {:>10}  {:>10} {:>10}\n",
            group_thousands(all.sites),
            megabytes(all.bytes),
            group_thousands(named.sites),
            megabytes(named.bytes)
        )
    };
    for (i, site) in seen.iter().enumerate() {
        if site.kind != Kind::Gone {
            all.add(site.bytes);
        }
        if site.kind == Kind::Named {
            named.add(site.bytes);
        }
        if MARKS.contains(&(i + 1)) && i + 1 < seen.len() {
            out.push_str(&row(group_thousands(i as u64 + 1), all, named));
        }
    }
    out.push_str(&row("all".to_string(), all, named));
    out
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

#[cfg(test)]
mod tests {
    use plumb_core::write_jsonl;

    use super::*;
    use crate::records::load_records;

    fn site(domain: &str, tranco: u32, title: Option<&str>) -> SiteRecord {
        let mut r = SiteRecord::new(domain);
        r.signals.tranco_rank = Some(tranco);
        r.title = title.map(str::to_string);
        r
    }

    #[test]
    fn the_best_named_sites_are_cut_out_and_sized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let mut gone = site("gone.com", 1, None);
        gone.gone_at = Some(5);
        let mut aliased = site("aliased.org", 4, None);
        aliased.aliases = vec!["Aliased".into()];
        write_jsonl(
            &path,
            &[
                gone,
                site("best.com", 2, Some("Best")),
                site("stub.net", 3, None),
                aliased,
                site("tail.com", 900, Some("Tail")),
            ],
        )
        .unwrap();
        let out = dir.path().join("top.jsonl");
        let args = TopSitesArgs {
            records: path.clone(),
            out: Some(out.clone()),
            top: Some(2),
            named_only: true,
        };
        run(&args).unwrap();
        let kept = load_records(&out).unwrap();
        let mut domains: Vec<&str> = kept.iter().map(|r| r.domain.as_str()).collect();
        domains.sort_unstable();
        assert_eq!(domains, ["aliased.org", "best.com"]);
        assert!(path.is_file(), "the records are left alone");

        let args = TopSitesArgs {
            named_only: false,
            top: None,
            ..args
        };
        run(&args).unwrap();
        assert_eq!(load_records(&out).unwrap().len(), 4, "all but the gone one");
    }

    #[test]
    fn the_report_counts_each_kind_and_the_best_sites() {
        let seen: Vec<Seen> = (0..150_000)
            .map(|i| Seen {
                score: -(i as f32),
                domain: format!("s{i}.com").into(),
                bytes: 10,
                kind: if i % 2 == 0 {
                    Kind::Named
                } else {
                    Kind::LinkOnly
                },
            })
            .collect();
        let report = report(&seen);
        assert!(
            report.starts_with("150,000 sites, 1.5 MB of records\n"),
            "{report}"
        );
        assert!(
            report.contains("  named            75,000 sites      0.8 MB"),
            "{report}"
        );
        // The 100,000 mark and the whole file; no mark past it.
        assert!(
            report.contains("100,000     100,000     1.0 MB      50,000     0.5 MB"),
            "{report}"
        );
        assert!(
            report.contains("all     150,000     1.5 MB      75,000     0.8 MB"),
            "{report}"
        );
        assert!(!report.contains("250,000"), "{report}");
    }
}
