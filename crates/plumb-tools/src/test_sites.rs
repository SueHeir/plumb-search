//! Sites for the tests of the commands that read and write records files:
//! a file holding them as nodes and hand-made files write them, so that a
//! test can tell that a command reads and writes each site as it did when
//! it held records.

use std::io::Write;
use std::path::Path;

use plumb_core::{stored, Headline, KeyPage, LinkText, Redirect, Signals, SiteRecord};

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
