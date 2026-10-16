//! Sites for the tests of the commands that read and write records files:
//! a file holding them as nodes and hand-made files write them, so that a
//! test can tell that a command reads and writes each site as it did when
//! it held records. [`load_records`] and [`write_sorted`] are what a
//! record set made of such a file before the commands held sites' stored
//! items, the reference their output is checked against.

use std::io::Write;
use std::path::Path;

use plumb_core::{
    site, stored, Headline, KeyPage, LinkText, RecordSet, Redirect, Signals, SiteRecord,
};
use plumb_format::Item;
use plumb_node::records::{journal_path, Change};

/// The site [`write`] writes as its stored item rather than a record.
const WRITTEN_AS_ITEM: &str = "rust-lang.org";

/// Records of a few sites, in the forms a records file can hold them: one
/// with every field a record has; one whose text, URLs and domain are not
/// in the form loading cleans them to, and the same site again; a domain
/// not in punycode; a bot check read in place of a homepage; a gone site,
/// one that redirects to another, ones with little or nothing but a name,
/// the network's own site; and a domain that is none.
pub(crate) fn records() -> Vec<SiteRecord> {
    let python = SiteRecord {
        domain: "python.org".into(),
        url: Some("https://www.python.org/".into()),
        title: Some("Welcome to Python.org".into()),
        description: Some("The official home of the Python Programming Language".into()),
        headings: vec!["Get Started".into(), "Download".into()],
        body_text: Some("Python is a programming language that lets you work quickly.".into()),
        terms: vec!["python".into(), "download python".into()],
        link_texts: vec![
            LinkText::from_linkers("python", 0x00a1_b2c3_d4e5_f607),
            LinkText::with_count("python.org", 3),
        ],
        aliases: vec!["Python".into(), "Python Software Foundation".into()],
        signals: Signals {
            harmonic_rank: Some(300),
            pagerank_rank: Some(250),
            tranco_rank: Some(512),
            linking_domains: 412,
            official_site: true,
            sitelinks: 150,
            fact_checks: 12,
            fact_agrees: 11,
        },
        crawled_at: Some(1_791_605_520),
        crawl_attempted_at: Some(1_791_605_600),
        country: Some("US".into()),
        kinds: vec!["programming language".into()],
        about: Some("general-purpose programming language".into()),
        intro: Some("Python is a high-level, general-purpose programming language.".into()),
        summary: Some("The home of the Python programming language.".into()),
        search_url: Some("https://www.python.org/search/?q={searchTerms}".into()),
        language: Some("en".into()),
        key_pages: vec![KeyPage {
            label: "Downloads".into(),
            url: "https://www.python.org/downloads/".into(),
        }],
        links_to: vec![
            "psf.org".into(),
            "pypi.org".into(),
            "rust-lang.org".into(),
            "example.com".into(),
        ],
        crawl_failures: 1,
        crawl_version: 7,
        gone_at: None,
        redirect: Some(Redirect {
            to: "python.org".into(),
            at: 1_791_605_520,
            inner: true,
        }),
        icon: Some("iVBORw0KGgo=".into()),
        news: vec![Headline {
            title: "Python 3.15 is out".into(),
            url: "https://www.python.org/news/".into(),
            at: 1_791_605_520,
        }],
    };

    let mut example = SiteRecord::new("Example.COM");
    example.url = Some("HTTPS://WWW.Example.com/index%2ehtml?".into());
    example.title = Some(" Example\u{7} Domain ".into());
    example.headings = vec![" Example Domain ".into()];
    example.aliases = vec![" Example".into(), "Example".into(), String::new()];
    example.link_texts = vec![
        LinkText::with_count("example ", 3),
        LinkText::from_linkers("example", 0x3),
    ];
    example.kinds = vec!["website ".into(), "website".into()];
    example.signals.tranco_rank = Some(900);
    example.signals.linking_domains = 30;
    example.crawled_at = Some(1_791_000_000);
    example.links_to = vec!["iana.org".into(), "foo_bar.com".into(), "python.org".into()];
    example.key_pages = vec![KeyPage {
        label: " More ".into(),
        url: "https://www.iana.org/domains/example".into(),
    }];
    let mut example_again = SiteRecord::new("example.com");
    example_again.aliases = vec!["Example Domain".into()];
    example_again.signals.pagerank_rank = Some(40);

    let mut acer = SiteRecord::new("acer.com");
    acer.title = Some(" Acer ".into());
    acer.aliases = vec!["Acer Inc.".into()];
    acer.link_texts = vec![
        LinkText::with_count("acer", 12),
        LinkText::from_linkers("laptops", 0b111),
    ];
    acer.kinds = vec!["company".into()];
    acer.country = Some("TW".into());
    acer.signals.tranco_rank = Some(2_000);
    acer.links_to = vec!["python.org".into(), "intel.com".into()];

    let mut stub = SiteRecord::new("stub.net");
    stub.signals.tranco_rank = Some(5_000);

    let mut gone = SiteRecord::new("gone.com");
    gone.signals.tranco_rank = Some(100);
    gone.crawled_at = Some(1_780_000_000);
    gone.crawl_attempted_at = Some(1_791_000_000);
    gone.crawl_failures = 6;
    gone.gone_at = Some(1_791_000_000);

    let mut pnc = SiteRecord::new("pncbank.com");
    pnc.title = Some("PNC Bank".into());
    pnc.signals.tranco_rank = Some(3_000);
    pnc.redirect = Some(Redirect {
        to: "pnc.com".into(),
        at: 1_791_000_000,
        inner: false,
    });

    let mut munich = SiteRecord::new("münchen.de");
    munich.title = Some("München".into());
    munich.aliases = vec!["Munich".into()];
    munich.signals.harmonic_rank = Some(5_000);
    munich.links_to = vec!["acer.com".into(), "example.com".into()];

    let mut check = SiteRecord::new("usbank.com");
    check.url = Some("https://www.google.com/sorry/index".into());
    check.title = Some("Sorry...".into());
    check.headings = vec!["About this page".into()];
    check.crawled_at = Some(1_791_605_520);
    check.signals.tranco_rank = Some(700);
    check.links_to = vec!["python.org".into()];

    let mut rust = SiteRecord::new(WRITTEN_AS_ITEM);
    rust.url = Some("https://www.rust-lang.org/".into());
    rust.title = Some("Rust Programming Language".into());
    rust.body_text = Some("A language empowering everyone.".into());
    rust.signals.tranco_rank = Some(1_500);
    rust.signals.official_site = true;
    rust.crawled_at = Some(1_791_600_000);
    rust.links_to = vec!["crates.io".into(), "python.org".into()];

    vec![
        python,
        example,
        acer,
        stub,
        gone,
        pnc,
        munich,
        check,
        rust,
        example_again,
        SiteRecord::new("not a domain"),
        plumb_core::home_site_record(),
    ]
}

/// Writes `records` to `path` as a records file, each on a line of its
/// own as the record's JSON, as nodes up to version 0.4 wrote them, but
/// for `rust-lang.org`, written as its stored item ([`stored::line`]) as
/// nodes write them now.
pub(crate) fn write(path: &Path, records: &[SiteRecord]) {
    let mut file = std::fs::File::create(path).unwrap();
    for record in records {
        let line = if record.domain == WRITTEN_AS_ITEM {
            stored::line(record).unwrap()
        } else {
            serde_json::to_vec(record).unwrap()
        };
        file.write_all(&line).unwrap();
        file.write_all(b"\n").unwrap();
    }
}

/// The records of the records file at `path` and its journal, as nodes
/// loaded them when they held records: each line read as a record and
/// merged into a record set, then each change the journal holds made to
/// the records as changes were made then, a damaged line skipped.
pub(crate) fn load_records(path: &Path) -> RecordSet {
    let mut set = RecordSet::new();
    for line in std::fs::read(path).unwrap().split(|&b| b == b'\n') {
        let line = line.trim_ascii();
        if !line.is_empty() {
            set.upsert(stored::read(line).unwrap());
        }
    }
    let journal = std::fs::read(journal_path(path)).unwrap_or_default();
    for line in journal.split(|&b| b == b'\n') {
        if let Ok(change) = serde_json::from_slice::<Change>(line.trim_ascii()) {
            apply(change, &mut set);
        }
    }
    set
}

/// Makes `change` to a record set, as changes were made when they carried
/// records: the change's site becomes the record it holds.
fn apply(change: Change, set: &mut RecordSet) {
    let record = |site: &Item| SiteRecord::from_stored_item(site).unwrap();
    match change {
        Change::Merge { site } => {
            set.upsert(record(&site));
        }
        Change::MergeShared { site } => {
            set.upsert_shared(record(&site));
        }
        Change::RefreshShared { site } => {
            let held = plumb_core::canonical_domain(site::domain(&site))
                .is_some_and(|domain| set.get(&domain).is_some());
            if held {
                set.upsert_shared(record(&site));
            }
        }
        Change::Mark {
            domain,
            attempted_at,
            failures,
        } => {
            if set.get(&domain).is_some() {
                let record = set.entry(&domain);
                record.crawl_attempted_at = attempted_at;
                record.crawl_failures = failures;
            }
        }
        Change::Gone { domain, at } => {
            if set.get(&domain).is_some() {
                set.entry(&domain).make_gone(at);
            }
        }
        Change::TakeBack { domain, names } => {
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            set.take_back_official_site(&domain, &names);
        }
        Change::SubdomainSite { site } => {
            let mut record = record(&site);
            let Some(parent) = plumb_core::parent_domain(&record.domain)
                .and_then(|parent| set.get(parent))
                .map(|parent| parent.signals.clone())
            else {
                return;
            };
            let signals = &mut record.signals;
            signals.tranco_rank = signals.tranco_rank.or(parent.tranco_rank);
            signals.harmonic_rank = signals.harmonic_rank.or(parent.harmonic_rank);
            signals.pagerank_rank = signals.pagerank_rank.or(parent.pagerank_rank);
            set.split_subdomain_sites(std::slice::from_ref(&record));
            set.upsert(record);
        }
    }
}

/// Writes `set` to `path` as a node wrote a whole record set: each record
/// as a stored line ([`stored::line`]), best link score first.
pub(crate) fn write_sorted(path: &Path, set: &RecordSet) {
    let mut out = Vec::new();
    for record in set.clone().into_sorted_vec() {
        out.extend(stored::line(&record).unwrap());
        out.push(b'\n');
    }
    std::fs::write(path, out).unwrap();
}

/// Records of more sites for the tools that size private search's
/// buckets and PIR tables, with what trimming them reads and cuts: a long
/// description and page text, headings, terms, Wikipedia's and Wikidata's
/// text, more link texts than a bucket keeps, and the node's own fields.
pub(crate) fn bucket_records() -> Vec<SiteRecord> {
    let mut usbank = SiteRecord::new("usbank.com");
    usbank.url = Some("https://www.usbank.com/".into());
    usbank.title = Some("U.S. Bank | Checking & Savings".into());
    usbank.description = Some(format!(
        "Checking and savings. {}",
        "Bank with us. ".repeat(30)
    ));
    usbank.headings = vec!["Personal banking".into(), "Business banking".into()];
    usbank.body_text = Some("Open an account today. ".repeat(20));
    usbank.terms = vec!["bank".into(), "bank".into(), "checking".into()];
    usbank.aliases = vec!["U.S. Bancorp".into()];
    usbank.link_texts = (0..12)
        .map(|i| LinkText::from_linkers(format!("us bank {i}"), (1u64 << i) - 1))
        .collect();
    usbank.signals.tranco_rank = Some(900);
    usbank.signals.linking_domains = 4000;
    usbank.signals.official_site = true;
    usbank.country = Some("US".into());
    usbank.kinds = vec!["bank".into()];
    usbank.about = Some("American bank holding company".into());
    usbank.intro = Some("U.S. Bancorp is an American bank holding company.".into());
    usbank.crawled_at = Some(1_791_605_520);
    usbank.crawl_attempted_at = Some(1_791_605_600);
    usbank.crawl_failures = 2;
    usbank.key_pages = vec![KeyPage {
        label: "Log in".into(),
        url: "https://www.usbank.com/login".into(),
    }];
    usbank.links_to = vec!["visa.com".into()];
    let mut chase = SiteRecord::new("chase.com");
    chase.title = Some("Chase Bank".into());
    chase.description = Some("Credit cards, mortgages and banking.".into());
    chase.signals.tranco_rank = Some(200);
    chase.link_texts = vec![LinkText::with_count("chase", 30)];
    let mut banks: Vec<SiteRecord> = (0..40u32)
        .map(|n| {
            let mut bank = SiteRecord::new(format!("bank{n}.example"));
            bank.title = Some(format!("Bank number {n}"));
            bank.description = Some(format!("The {n}th bank. ").repeat(n as usize % 7 + 1));
            bank.signals.tranco_rank = Some(10_000 + n);
            bank
        })
        .collect();
    banks.extend([usbank, chase]);
    banks
}

/// Checks `text` against the golden file at `path`, or writes it there
/// when `PLUMB_WRITE_GOLDEN` is set, as plumb-node's golden tests do.
pub(crate) fn check_golden(path: &Path, text: &str) {
    if std::env::var_os("PLUMB_WRITE_GOLDEN").is_some() {
        std::fs::write(path, text).unwrap();
        return;
    }
    let golden = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    let (mut want, mut got) = (golden.lines(), text.lines());
    for n in 1.. {
        match (want.next(), got.next()) {
            (None, None) => break,
            (want, got) if want == got => continue,
            (want, got) => panic!(
                "{}:{n} differs\n  golden: {}\n  now:    {}",
                path.display(),
                want.unwrap_or("(the end)"),
                got.unwrap_or("(the end)")
            ),
        }
    }
}
