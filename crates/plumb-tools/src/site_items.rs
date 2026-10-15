//! `plumb-tools site-items`: what a records file's sites would lose as
//! site items ([`plumb_core::site_item`]), changing nothing. For each field
//! of a site, how many sites read back from their item without it or with
//! it changed; how many sites make no item at all; the bytes of both
//! shapes, the time to read them, and the memory a sample of sites takes
//! held in each. And how many sites cleaning changes
//! ([`SiteRecord::clean`]), and the time it takes, first and once clean;
//! and the bytes of the stored items a node would write
//! ([`plumb_core::stored::item_line`]), the time to read them, and how many
//! do not read back as their clean site.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use plumb_core::SiteRecord;
use plumb_format::Item;
use plumb_node::web::group_thousands;
use serde_json::{Map, Value};

#[derive(Debug, Args)]
pub struct SiteItemsArgs {
    /// Records file (JSON lines), as `records.jsonl` in a node's data
    /// directory. Its journal is not read.
    #[arg(long, value_name = "PATH")]
    pub records: PathBuf,
    /// How many sites to name for each field that changes.
    #[arg(long, value_name = "N", default_value_t = 5)]
    pub examples: usize,
    /// With each example, what changed: the elements of a list only one
    /// side has, or where two texts part.
    #[arg(long)]
    pub diffs: bool,
    /// How many of the first sites to hold in memory in each shape, each
    /// in a process of its own, to measure what a site takes; 0 for none.
    #[arg(long, value_name = "N", default_value_t = 100_000)]
    pub memory_sample: usize,
    /// Only hold the sample in this shape and print the memory it took.
    #[arg(long, value_enum, hide = true)]
    pub hold: Option<Shape>,
}

/// A shape a site is held in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Shape {
    Record,
    Item,
}

/// The members of a site its item leaves out on purpose: the node's own
/// bookkeeping, and icons and headlines, which travel on their own.
const LEFT_OUT: [&str; 5] = [
    "crawl_attempted_at",
    "crawl_failures",
    "gone_at",
    "icon",
    "news",
];

/// How one field of a site fares through its item.
#[derive(Debug, Default)]
struct Field {
    /// Read back empty when the site had it.
    dropped: u64,
    /// Read back different.
    changed: u64,
    /// Sites it changed in, each with what changed when asked for.
    examples: Vec<(String, Option<String>)>,
}

#[derive(Debug, Default)]
struct Report {
    sites: u64,
    unreadable: u64,
    /// Sites that make no item, by the first words of why.
    no_item: BTreeMap<String, (u64, Vec<String>)>,
    /// Sites that read back the same in every field.
    same: u64,
    fields: BTreeMap<String, Field>,
    record_bytes: u64,
    item_bytes: u64,
    record_read: Duration,
    item_read: Duration,
    /// Sites [`SiteRecord::clean`] changes.
    cleaned: u64,
    clean_time: Duration,
    /// The time to clean sites clean already, as a node does each time it
    /// reads its records once they are.
    clean_again_time: Duration,
    stored_bytes: u64,
    stored_read: Duration,
    /// Sites whose stored item does not read back as the clean site, with
    /// the first few.
    stored_differ: (u64, Vec<String>),
}

/// Reads every site of the records file as its item and back, and prints
/// what changed.
pub fn run(args: &SiteItemsArgs) -> Result<()> {
    if let Some(shape) = args.hold {
        return hold(args, shape);
    }
    let file =
        File::open(&args.records).with_context(|| format!("opening {}", args.records.display()))?;
    let mut report = Report::default();
    for line in BufReader::new(file).lines() {
        let line = line.context("reading the records file")?;
        if line.trim().is_empty() {
            continue;
        }
        report.add(&line, args);
    }
    report.print();
    if args.memory_sample > 0 {
        let record = held_bytes(args, Shape::Record)?;
        let item = held_bytes(args, Shape::Item)?;
        let per = |bytes: u64| bytes / args.memory_sample.max(1) as u64;
        println!(
            "memory, {} sites held: {} bytes a site as records, {} as items",
            group_thousands(args.memory_sample as u64),
            group_thousands(per(record)),
            group_thousands(per(item))
        );
    }
    Ok(())
}

impl Report {
    fn add(&mut self, line: &str, args: &SiteItemsArgs) {
        let examples = args.examples;
        let started = Instant::now();
        let Ok(record) = plumb_core::stored::read(line.as_bytes()) else {
            self.unreadable += 1;
            return;
        };
        self.record_read += started.elapsed();
        self.sites += 1;
        self.record_bytes += line.len() as u64 + 1;
        let mut clean = record.clone();
        let started = Instant::now();
        clean.clean();
        self.clean_time += started.elapsed();
        self.cleaned += u64::from(clean != record);
        let started = Instant::now();
        clean.clean();
        self.clean_again_time += started.elapsed();
        if let Ok(stored) = plumb_core::stored::item_line(&record) {
            self.stored_bytes += stored.len() as u64 + 1;
            let started = Instant::now();
            let back = plumb_core::stored::read(&stored);
            self.stored_read += started.elapsed();
            if back.ok().as_ref() != Some(&clean) {
                self.stored_differ.0 += 1;
                if self.stored_differ.1.len() < examples {
                    self.stored_differ.1.push(record.domain.clone());
                }
            }
        }
        let item_line = match record.to_line() {
            Ok(item_line) => item_line,
            Err(err) => {
                let why = format!("{err:#}");
                let why = why.split_whitespace().skip(1).take(4).collect::<Vec<_>>();
                let entry = self.no_item.entry(why.join(" ")).or_default();
                entry.0 += 1;
                if entry.1.len() < examples {
                    entry.1.push(record.domain);
                }
                return;
            }
        };
        self.item_bytes += item_line.len() as u64 + 1;
        let started = Instant::now();
        let back = serde_json::from_str::<Item>(&item_line)
            .map_err(anyhow::Error::from)
            .and_then(|item| SiteRecord::from_item(&item));
        self.item_read += started.elapsed();
        let Ok(back) = back else {
            self.no_item
                .entry("does not read back".into())
                .or_default()
                .0 += 1;
            return;
        };
        let (Ok(Value::Object(before)), Ok(Value::Object(after))) =
            (serde_json::to_value(&record), serde_json::to_value(&back))
        else {
            return;
        };
        let mut same = true;
        for (name, was, now) in members(&before, &after) {
            if LEFT_OUT.contains(&name.as_str()) || was == now {
                continue;
            }
            same = false;
            let field = self.fields.entry(name).or_default();
            if is_empty(now) {
                field.dropped += 1;
            } else {
                field.changed += 1;
            }
            if field.examples.len() < examples {
                let diff = args.diffs.then(|| difference(was, now));
                field.examples.push((record.domain.clone(), diff));
            }
        }
        self.same += u64::from(same);
    }

    fn print(&self) {
        let n = group_thousands;
        println!(
            "{} sites ({} lines unreadable); {} read back the same from their item",
            n(self.sites),
            n(self.unreadable),
            n(self.same)
        );
        if self.no_item.is_empty() {
            println!("  sites that make no item: 0");
        }
        for (why, (count, domains)) in &self.no_item {
            println!("  no item, {why}: {} ({})", n(*count), domains.join(", "));
        }
        println!("field: read back empty, read back different (sites)");
        for (name, field) in &self.fields {
            let domains: Vec<&str> = field.examples.iter().map(|(d, _)| d.as_str()).collect();
            println!(
                "  {name}: {}, {} ({})",
                n(field.dropped),
                n(field.changed),
                domains.join(", ")
            );
            for (domain, diff) in &field.examples {
                if let Some(diff) = diff {
                    println!("    {domain}: {diff}");
                }
            }
        }
        let ms = |time: Duration| time.as_millis() as u64;
        println!(
            "bytes: {} as records, {} as items; read in {} ms as records, {} ms as items",
            n(self.record_bytes),
            n(self.item_bytes),
            n(ms(self.record_read)),
            n(ms(self.item_read))
        );
        println!(
            "cleaning: {} sites change; {} ms, {} ms once clean",
            n(self.cleaned),
            n(ms(self.clean_time)),
            n(ms(self.clean_again_time))
        );
        println!(
            "stored items: {} bytes, read in {} ms; {} do not read back as their clean site ({})",
            n(self.stored_bytes),
            n(ms(self.stored_read)),
            n(self.stored_differ.0),
            self.stored_differ.1.join(", ")
        );
    }
}

/// Each member of two sites as JSON, with `signals` split into its own
/// members; a member one leaves out is null there.
fn members<'a>(before: &'a Map<String, Value>, after: &'a Map<String, Value>) -> Vec<Member<'a>> {
    let mut out = pairs("", before, after);
    out.retain(|(name, _, _)| name != "signals");
    if let (Some(Value::Object(a)), Some(Value::Object(b))) =
        (before.get("signals"), after.get("signals"))
    {
        out.extend(pairs("signals.", a, b));
    }
    out
}

/// A member's name, and its value before and after.
type Member<'a> = (String, &'a Value, &'a Value);

/// Each member of either object, named with `prefix`.
fn pairs<'a>(
    prefix: &str,
    a: &'a Map<String, Value>,
    b: &'a Map<String, Value>,
) -> Vec<Member<'a>> {
    let names = a.keys().chain(b.keys().filter(|key| !a.contains_key(*key)));
    names
        .map(|name| {
            let value = |map: &'a Map<String, Value>| map.get(name).unwrap_or(&Value::Null);
            (format!("{prefix}{name}"), value(a), value(b))
        })
        .collect()
}

/// What differs between a member's value before and after, in short: the
/// elements of a list only one side has, or two texts from where they
/// part, escaped so that a change of spaces or Unicode form shows.
fn difference(was: &Value, now: &Value) -> String {
    match (was, now) {
        (Value::Array(a), Value::Array(b)) => {
            let only = |x: &[Value], y: &[Value]| {
                let only: Vec<&Value> = x.iter().filter(|v| !y.contains(v)).collect();
                shortened(&serde_json::to_string(&only).unwrap_or_default())
            };
            if a.iter().all(|v| b.contains(v)) && b.iter().all(|v| a.contains(v)) {
                return format!("same {} elements in another order or count", a.len());
            }
            format!("lost {}, gained {}", only(a, b), only(b, a))
        }
        (Value::String(a), Value::String(b)) => {
            let at = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
            let from = at.saturating_sub(10);
            let part =
                |s: &str| shortened(&format!("{:?}", s.chars().skip(from).collect::<String>()));
            format!(
                "{} chars to {}, parting at char {at}: {} to {}",
                a.chars().count(),
                b.chars().count(),
                part(a),
                part(b)
            )
        }
        _ => format!(
            "{} to {}",
            shortened(&was.to_string()),
            shortened(&now.to_string())
        ),
    }
}

/// `text` cut to its first 120 characters.
fn shortened(text: &str) -> String {
    const SHOWN: usize = 120;
    if text.chars().count() <= SHOWN {
        return text.to_string();
    }
    format!("{}...", text.chars().take(SHOWN).collect::<String>())
}

/// Whether a member says nothing.
fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(list) => list.is_empty(),
        Value::Object(map) => map.is_empty(),
    }
}

/// The memory the first `--memory-sample` sites take held in `shape`,
/// each measured in a process of its own so that memory one shape freed
/// is not counted for the other. Items are written to a file first, so
/// the process holding them makes none.
fn held_bytes(args: &SiteItemsArgs, shape: Shape) -> Result<u64> {
    let (lines, name) = match shape {
        Shape::Record => (args.records.clone(), "record"),
        Shape::Item => (sample_items(args)?, "item"),
    };
    let output = Command::new(std::env::current_exe()?)
        .arg("site-items")
        .arg("--records")
        .arg(&lines)
        .args(["--memory-sample", &args.memory_sample.to_string()])
        .args(["--hold", name])
        .output();
    if shape == Shape::Item {
        let _ = std::fs::remove_file(&lines);
    }
    let output = output.context("measuring memory")?;
    if !output.status.success() {
        bail!(
            "measuring memory: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim().parse().context("reading the memory measured")
}

/// The first `--memory-sample` sites as item lines, in a file of their
/// own in the system's temporary directory.
fn sample_items(args: &SiteItemsArgs) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("plumb-site-items-{}.jsonl", std::process::id()));
    let mut out = std::io::BufWriter::new(File::create(&path)?);
    for line in sample(&args.records, args.memory_sample)? {
        let Ok(record) = plumb_core::stored::read(line.as_bytes()) else {
            continue;
        };
        if let Ok(item) = record.to_line() {
            writeln!(out, "{item}")?;
        }
    }
    out.flush()?;
    Ok(path)
}

/// The first `count` lines of a JSON lines file that are not blank.
fn sample(path: &PathBuf, count: usize) -> Result<Vec<String>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    Ok(BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .take(count)
        .collect())
}

/// Holds the lines of `--records` read as `shape` and prints how many
/// bytes the process grew by.
fn hold(args: &SiteItemsArgs, shape: Shape) -> Result<()> {
    let lines = sample(&args.records, args.memory_sample)?;
    let before = resident_bytes()?;
    let records: Vec<SiteRecord> = match shape {
        Shape::Record => lines
            .iter()
            .filter_map(|l| plumb_core::stored::read(l.as_bytes()).ok())
            .collect(),
        Shape::Item => Vec::new(),
    };
    let items: Vec<Item> = match shape {
        Shape::Record => Vec::new(),
        Shape::Item => lines
            .iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect(),
    };
    let after = resident_bytes()?;
    println!("{}", after.saturating_sub(before));
    drop((records, items));
    Ok(())
}

/// The process's resident memory, from `/proc/self/status` (Linux).
fn resident_bytes() -> Result<u64> {
    let status = std::fs::read_to_string("/proc/self/status")
        .context("reading /proc/self/status (Linux only)")?;
    let kb = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
        .context("no VmRSS in /proc/self/status")?;
    Ok(kb * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_what_sites_lose_as_items() {
        let mut same = SiteRecord::new("example.com");
        same.title = Some("Example".into());
        same.crawled_at = Some(1_800_000_000);
        // Bookkeeping an item leaves out on purpose is not counted.
        same.crawl_failures = 2;
        let mut long = SiteRecord::new("long.example");
        long.body_text = Some("word ".repeat(400));
        long.signals.linking_domains = 3;
        long.signals.sitelinks = 0;
        let mut spaced = SiteRecord::new("spaced.example");
        spaced.title = Some(" Spaced ".into());
        spaced.headings = vec![" Get Started ".into(), "Docs".into()];
        // Over the format's 16 KiB with nothing to shed.
        let mut big = SiteRecord::new("big.example");
        big.description = Some("a ".repeat(10_000));
        let args = SiteItemsArgs {
            records: PathBuf::new(),
            examples: 5,
            diffs: true,
            memory_sample: 0,
            hold: None,
        };
        let mut report = Report::default();
        for record in [same, long, spaced, big] {
            report.add(&serde_json::to_string(&record).unwrap(), &args);
        }
        report.add("not json", &args);
        assert_eq!((report.sites, report.unreadable, report.same), (4, 1, 1));
        assert_eq!(report.no_item.values().map(|(n, _)| n).sum::<u64>(), 1);
        let text = &report.fields["body_text"];
        assert_eq!((text.dropped, text.changed), (0, 1));
        let (domain, diff) = &text.examples[0];
        assert_eq!(domain, "long.example");
        assert!(diff
            .as_ref()
            .unwrap()
            .starts_with("2000 chars to 999, parting at char 999: "));
        let title = &report.fields["title"];
        assert_eq!(title.changed, 1);
        assert_eq!(
            title.examples[0].1.as_deref(),
            Some(r#"8 chars to 6, parting at char 0: " Spaced " to "Spaced""#)
        );
        assert_eq!(
            report.fields["headings"].examples[0].1.as_deref(),
            Some(r#"lost [" Get Started "], gained ["Get Started"]"#)
        );
        assert!(!report.fields.contains_key("signals.linking_domains"));
        assert!(!report.fields.contains_key("crawl_failures"));
        assert!(report.item_bytes > 0 && report.record_bytes > report.item_bytes / 2);
        // Trailing and leading spaces come off long, spaced and big.
        assert_eq!(report.cleaned, 3);
        // Stored items keep all four whole, big included.
        assert_eq!(report.stored_differ.0, 0);
        assert!(report.stored_bytes > 0);
    }
}
