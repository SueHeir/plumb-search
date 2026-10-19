//! Sites for the tests of the commands that read and write records files:
//! a file holding them as nodes and hand-made files write them, each of
//! the forms a records file can hold a site in.

use std::io::Write;
use std::path::Path;

use plumb_core::site::{self, CrawlMarks};
use plumb_core::sites::Sites;
use plumb_core::{stored, Headline, KeyPage, LinkText, Redirect, Signals};
use plumb_format::Item;

/// The site [`write`] writes as its stored item rather than a record.
const WRITTEN_AS_ITEM: &str = "rust-lang.org";

/// A few sites, in the forms a records file can hold them: one with every
/// field a site has; one whose text, URLs and domain are not in the form
/// loading cleans them to, and the same site again; a domain not in
/// punycode; a bot check read in place of a homepage; a gone site, one that
/// redirects to another, ones with little or nothing but a name, the
/// network's own site; and a domain that is none.
pub(crate) fn sites() -> Vec<Item> {
    let mut python = site::new("python.org");
    python.url = Some("https://www.python.org/".into());
    python.title = Some("Welcome to Python.org".into());
    python.about = Some("The official home of the Python Programming Language".into());
    site::set_headings(&mut python, vec!["Get Started".into(), "Download".into()]);
    python.text = Some("Python is a programming language that lets you work quickly.".into());
    site::set_terms(&mut python, vec!["python".into(), "download python".into()]);
    let link_texts = [
        LinkText::from_linkers("python", 0x00a1_b2c3_d4e5_f607),
        LinkText::with_count("python.org", 3),
    ];
    site::set_link_texts(&mut python, &link_texts);
    python.names = vec!["Python".into(), "Python Software Foundation".into()];
    let signals = Signals {
        harmonic_rank: Some(300),
        pagerank_rank: Some(250),
        tranco_rank: Some(512),
        linking_domains: 412,
        official_site: true,
        sitelinks: 150,
        fact_checks: 12,
        fact_agrees: 11,
    };
    site::set_signals(&mut python, &signals);
    site::set_crawled(&mut python, Some(1_791_605_520));
    let marks = CrawlMarks {
        tried: Some(1_791_605_600),
        failures: 1,
        gone: None,
    };
    site::set_crawl_marks(&mut python, marks);
    site::set_country(&mut python, Some("US".into()));
    site::set_kinds(&mut python, vec!["programming language".into()]);
    let about = "general-purpose programming language";
    site::set_wikidata_about(&mut python, Some(about.into()));
    let intro = "Python is a high-level, general-purpose programming language.";
    site::set_intro(&mut python, Some(intro.into()));
    let summary = "The home of the Python programming language.";
    site::set_summary(&mut python, Some(summary.into()));
    let search = "https://www.python.org/search/?q={searchTerms}";
    site::set_search_url(&mut python, Some(search.into()));
    python.lang = Some("en".into());
    let downloads = KeyPage {
        label: "Downloads".into(),
        url: "https://www.python.org/downloads/".into(),
    };
    site::set_key_pages(&mut python, vec![downloads]);
    let links = ["psf.org", "pypi.org", "rust-lang.org", "example.com"];
    site::set_links_to(&mut python, links.map(String::from).to_vec());
    site::set_crawl_version(&mut python, 7);
    let redirect = Redirect {
        to: "python.org".into(),
        at: 1_791_605_520,
        inner: true,
    };
    site::set_redirect(&mut python, Some(redirect));
    site::set_icon(&mut python, Some("iVBORw0KGgo=".into()));
    let news = Headline {
        title: "Python 3.15 is out".into(),
        url: "https://www.python.org/news/".into(),
        at: 1_791_605_520,
    };
    site::set_news(&mut python, vec![news]);

    let mut example = site::new("Example.COM");
    example.url = Some("HTTPS://WWW.Example.com/index%2ehtml?".into());
    example.title = Some(" Example\u{7} Domain ".into());
    site::set_headings(&mut example, vec![" Example Domain ".into()]);
    example.names = vec![" Example".into(), "Example".into(), String::new()];
    let link_texts = [
        LinkText::with_count("example ", 3),
        LinkText::from_linkers("example", 0x3),
    ];
    site::set_link_texts(&mut example, &link_texts);
    site::set_kinds(&mut example, vec!["website ".into(), "website".into()]);
    ranks(&mut example, |signals| {
        signals.tranco_rank = Some(900);
        signals.linking_domains = 30;
    });
    site::set_crawled(&mut example, Some(1_791_000_000));
    let links = ["iana.org", "foo_bar.com", "python.org"];
    site::set_links_to(&mut example, links.map(String::from).to_vec());
    let more = KeyPage {
        label: " More ".into(),
        url: "https://www.iana.org/domains/example".into(),
    };
    site::set_key_pages(&mut example, vec![more]);
    let mut example_again = site::new("example.com");
    example_again.names = vec!["Example Domain".into()];
    ranks(&mut example_again, |signals| {
        signals.pagerank_rank = Some(40)
    });

    let mut acer = site::new("acer.com");
    acer.title = Some(" Acer ".into());
    acer.names = vec!["Acer Inc.".into()];
    let link_texts = [
        LinkText::with_count("acer", 12),
        LinkText::from_linkers("laptops", 0b111),
    ];
    site::set_link_texts(&mut acer, &link_texts);
    site::set_kinds(&mut acer, vec!["company".into()]);
    site::set_country(&mut acer, Some("TW".into()));
    ranks(&mut acer, |signals| signals.tranco_rank = Some(2_000));
    let links = ["python.org", "intel.com"];
    site::set_links_to(&mut acer, links.map(String::from).to_vec());

    let mut stub = site::new("stub.net");
    ranks(&mut stub, |signals| signals.tranco_rank = Some(5_000));

    let mut gone = site::new("gone.com");
    ranks(&mut gone, |signals| signals.tranco_rank = Some(100));
    site::set_crawled(&mut gone, Some(1_780_000_000));
    let marks = CrawlMarks {
        tried: Some(1_791_000_000),
        failures: 6,
        gone: Some(1_791_000_000),
    };
    site::set_crawl_marks(&mut gone, marks);

    let mut pnc = site::new("pncbank.com");
    pnc.title = Some("PNC Bank".into());
    ranks(&mut pnc, |signals| signals.tranco_rank = Some(3_000));
    let redirect = Redirect {
        to: "pnc.com".into(),
        at: 1_791_000_000,
        inner: false,
    };
    site::set_redirect(&mut pnc, Some(redirect));

    let mut munich = site::new("münchen.de");
    munich.title = Some("München".into());
    munich.names = vec!["Munich".into()];
    ranks(&mut munich, |signals| signals.harmonic_rank = Some(5_000));
    let links = ["acer.com", "example.com"];
    site::set_links_to(&mut munich, links.map(String::from).to_vec());

    let mut check = site::new("usbank.com");
    check.url = Some("https://www.google.com/sorry/index".into());
    check.title = Some("Sorry...".into());
    site::set_headings(&mut check, vec!["About this page".into()]);
    site::set_crawled(&mut check, Some(1_791_605_520));
    ranks(&mut check, |signals| signals.tranco_rank = Some(700));
    site::set_links_to(&mut check, vec!["python.org".into()]);

    let mut rust = site::new(WRITTEN_AS_ITEM);
    rust.url = Some("https://www.rust-lang.org/".into());
    rust.title = Some("Rust Programming Language".into());
    rust.text = Some("A language empowering everyone.".into());
    ranks(&mut rust, |signals| {
        signals.tranco_rank = Some(1_500);
        signals.official_site = true;
    });
    site::set_crawled(&mut rust, Some(1_791_600_000));
    let links = ["crates.io", "python.org"];
    site::set_links_to(&mut rust, links.map(String::from).to_vec());

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
        site::new("not a domain"),
        plumb_core::home_site(),
    ]
}

/// Changes `site`'s ranks and other signals with `change`.
pub(crate) fn ranks(site: &mut Item, change: impl FnOnce(&mut Signals)) {
    let mut signals = site::signals(site);
    change(&mut signals);
    site::set_signals(site, &signals);
}

/// Writes `sites` to `path` as a records file, each on a line of its own
/// as the record's JSON ([`stored::record_json_of`]), as nodes up to
/// version 0.4 wrote them, but for `rust-lang.org`, written as its stored
/// item ([`stored::item_as_line`]) as nodes write them now.
pub(crate) fn write(path: &Path, sites: &[Item]) {
    let mut file = std::fs::File::create(path).unwrap();
    for site in sites {
        let line = if site.id == WRITTEN_AS_ITEM {
            stored::item_as_line(site).unwrap()
        } else {
            stored::record_json_of(site).unwrap().into_bytes()
        };
        file.write_all(&line).unwrap();
        file.write_all(b"\n").unwrap();
    }
}

/// Writes `sites` to `path` as a node writes a whole set: each site's
/// stored line, best link score first.
pub(crate) fn write_sorted(path: &Path, sites: &Sites) {
    let mut out = Vec::new();
    for line in sites.lines_by_link_score() {
        out.extend(line);
        out.push(b'\n');
    }
    std::fs::write(path, out).unwrap();
}

/// More sites for the tools that size private search's buckets and PIR
/// tables, with what trimming them reads and cuts: a long description and
/// page text, headings, terms, Wikipedia's and Wikidata's text, more link
/// texts than a bucket keeps, and the node's own fields.
pub(crate) fn bucket_sites() -> Vec<Item> {
    let mut usbank = site::new("usbank.com");
    usbank.url = Some("https://www.usbank.com/".into());
    usbank.title = Some("U.S. Bank | Checking & Savings".into());
    usbank.about = Some(format!(
        "Checking and savings. {}",
        "Bank with us. ".repeat(30)
    ));
    let headings = vec!["Personal banking".into(), "Business banking".into()];
    site::set_headings(&mut usbank, headings);
    usbank.text = Some("Open an account today. ".repeat(20));
    let terms = vec!["bank".into(), "bank".into(), "checking".into()];
    site::set_terms(&mut usbank, terms);
    usbank.names = vec!["U.S. Bancorp".into()];
    let link_texts: Vec<LinkText> = (0..12)
        .map(|i| LinkText::from_linkers(format!("us bank {i}"), (1u64 << i) - 1))
        .collect();
    site::set_link_texts(&mut usbank, &link_texts);
    ranks(&mut usbank, |signals| {
        signals.tranco_rank = Some(900);
        signals.linking_domains = 4000;
        signals.official_site = true;
    });
    site::set_country(&mut usbank, Some("US".into()));
    site::set_kinds(&mut usbank, vec!["bank".into()]);
    let about = "American bank holding company";
    site::set_wikidata_about(&mut usbank, Some(about.into()));
    let intro = "U.S. Bancorp is an American bank holding company.";
    site::set_intro(&mut usbank, Some(intro.into()));
    site::set_crawled(&mut usbank, Some(1_791_605_520));
    let marks = CrawlMarks {
        tried: Some(1_791_605_600),
        failures: 2,
        gone: None,
    };
    site::set_crawl_marks(&mut usbank, marks);
    let login = KeyPage {
        label: "Log in".into(),
        url: "https://www.usbank.com/login".into(),
    };
    site::set_key_pages(&mut usbank, vec![login]);
    site::set_links_to(&mut usbank, vec!["visa.com".into()]);
    let mut chase = site::new("chase.com");
    chase.title = Some("Chase Bank".into());
    chase.about = Some("Credit cards, mortgages and banking.".into());
    ranks(&mut chase, |signals| signals.tranco_rank = Some(200));
    site::set_link_texts(&mut chase, &[LinkText::with_count("chase", 30)]);
    let mut banks: Vec<Item> = (0..40u32)
        .map(|n| {
            let mut bank = site::new(format!("bank{n}.example"));
            bank.title = Some(format!("Bank number {n}"));
            bank.about = Some(format!("The {n}th bank. ").repeat(n as usize % 7 + 1));
            ranks(&mut bank, |signals| signals.tranco_rank = Some(10_000 + n));
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
