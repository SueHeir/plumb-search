//! What private search hands a browser and what the browser ranks of it,
//! checked against two golden files beside this one:
//! `private_buckets_golden.txt`, the bytes `GET /api/buckets/{table}/{n}`
//! answers with for each bucket the searches here fetch, and
//! `private_golden.txt`, what plumb-private ranks for each search: of the
//! buckets fetched from this node, and of the same buckets sealed to other
//! nodes, each asked of two, one of which lies about popularity and points
//! its sites elsewhere. The sites hold what trimming and ranking read:
//! names, titles with parts, page text, terms, headings, Wikidata's and
//! Wikipedia's text, a summary, a homepage borrowed from another site,
//! more link texts than a bucket keeps, every rank, kinds, countries,
//! languages, adult words, a sign-in page, an IDN, and the node's own
//! fields that trimming drops (crawl marks, icon, headlines, key pages,
//! linked sites). Scores are written to 3 decimal places, as their last
//! bit can differ from one run to the next.
//!
//! Both were written on trimmed records, before private search moved onto
//! sites' items (step 10 of the data format plan), so that the ranking on
//! items is checked against it. `private_golden.txt` passed unchanged on
//! items. The buckets changed form, from trimmed records' JSON to trimmed
//! items, and `private_buckets_golden.txt` was written again then; what it
//! held before is kept in `private_buckets_records.txt`, never written
//! again, as what a browser may still have cached of a node of an earlier
//! version, and ranking that gives `private_golden.txt` too. Never
//! written again to make a test pass: `PLUMB_WRITE_GOLDEN=1 cargo test -p
//! plumb-node --lib private_golden` writes them.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use plumb_core::keys::pick_buckets;
use plumb_core::{
    now_unix, site, stored, Headline, KeyPage, LinkText, Operators, Redirect, SafeSearch, Signals,
    SiteRecord,
};
use plumb_net::oblivious::{seal_response, Gateway, Opened};
use plumb_net::proto::{BucketRecord, BucketResponse};
use plumb_net::{BucketTable, BucketWriter};
use plumb_private::sealed::{self, Target};
use plumb_private::Options;

use super::super::private::slim_bucket;

/// When the sites here were crawled.
const CRAWLED: u64 = 1_792_000_000;

fn crawled(domain: &str, title: &str, tranco: Option<u32>, linking: u32) -> SiteRecord {
    let mut record = SiteRecord::new(domain);
    record.url = Some(format!("https://www.{domain}/"));
    record.title = Some(title.into());
    record.crawled_at = Some(CRAWLED);
    record.signals = Signals {
        tranco_rank: tranco,
        linking_domains: linking,
        ..Signals::default()
    };
    record
}

/// The sites the node holds.
fn sites() -> Vec<SiteRecord> {
    let mut usbank = crawled(
        "usbank.com",
        "U.S. Bank | Checking & Savings - Personal Banking",
        Some(900),
        4000,
    );
    usbank.description = Some("Checking, savings and home loans from U.S. Bank.".into());
    usbank.headings = vec!["Personal banking".into(), "Business banking".into()];
    usbank.body_text = Some("Bank with us. ".repeat(40));
    usbank.terms = vec!["bank".into(), "bank".into(), "checking".into()];
    usbank.aliases = vec!["U.S. Bancorp".into(), "US Bank".into()];
    usbank.link_texts = (0..12)
        .map(|i| LinkText::from_linkers(format!("us bank {i}"), (1u64 << i) - 1))
        .chain([LinkText::from_linkers("us bank", u64::MAX >> 4)])
        .collect();
    usbank.signals.harmonic_rank = Some(800);
    usbank.signals.pagerank_rank = Some(1000);
    usbank.signals.official_site = true;
    usbank.signals.sitelinks = 40;
    usbank.signals.fact_checks = 12;
    usbank.signals.fact_agrees = 11;
    usbank.country = Some("US".into());
    usbank.kinds = vec!["bank".into()];
    usbank.about = Some("American bank holding company".into());
    usbank.intro = Some("U.S. Bancorp is an American bank holding company.".into());
    usbank.search_url = Some("https://www.usbank.com/search?q={searchTerms}".into());
    usbank.language = Some("en".into());
    usbank.crawl_attempted_at = Some(CRAWLED + 60);
    usbank.crawl_failures = 1;
    usbank.crawl_version = 3;
    usbank.icon = Some("iVBORw0KGgo=".into());
    usbank.news = vec![Headline {
        title: "Rates go up".into(),
        url: "https://www.usbank.com/news/rates".into(),
        at: CRAWLED,
    }];
    usbank.key_pages = vec![KeyPage {
        label: "Log in".into(),
        url: "https://www.usbank.com/login".into(),
    }];
    usbank.links_to = vec!["visa.com".into(), "mastercard.com".into()];

    let mut chase = crawled(
        "chase.com",
        "Chase Bank - Credit Cards, Mortgages, Banking",
        Some(200),
        9000,
    );
    chase.country = Some("US".into());
    chase.kinds = vec!["bank".into()];
    chase.link_texts = vec![
        LinkText::with_count("chase", 30),
        LinkText::with_count("chase login", 4),
    ];
    chase.key_pages = vec![KeyPage {
        label: "Sign in".into(),
        url: "https://www.chase.com/signin".into(),
    }];
    chase.signals.official_site = true;

    let mut america = crawled(
        "bankofamerica.com",
        "Bank of America - Banking, Credit Cards, Loans",
        Some(300),
        8000,
    );
    america.aliases = vec!["Bank of America".into(), "BofA".into()];
    america.signals.official_site = true;
    america.country = Some("US".into());
    america.kinds = vec!["bank".into()];

    let mut dkb = crawled("dkb.de", "DKB - Das Kundenkonto", Some(5000), 800);
    dkb.kinds = vec!["bank".into()];
    dkb.language = Some("de".into());
    dkb.description = Some("Ihr Girokonto bei der DKB".into());

    let mut deutsche = crawled("deutsche-bank.de", "Deutsche Bank", Some(4000), 900);
    deutsche.aliases = vec!["Deutsche Bank AG".into()];
    deutsche.kinds = vec!["bank".into()];
    deutsche.language = Some("de".into());
    deutsche.country = Some("DE".into());
    deutsche.headings = vec!["Konto und Karte".into()];

    let mut irs = crawled("irs.gov", "Internal Revenue Service", Some(300), 7000);
    irs.description = Some("Get a tax refund and pay taxes".into());
    irs.about = Some("United States federal tax agency".into());
    irs.terms = vec!["refund".into(), "taxes".into()];

    let mut tracker = crawled("refundtracker.net", "IRS refund tracker", Some(400_000), 3);
    tracker.description = Some("Where is my tax refund?".into());

    let mut lookalike = crawled(
        "usbank-login-help.com",
        "US Bank Login Help | us bank login",
        None,
        0,
    );
    lookalike.url = Some("https://usbank-login-help.com/login".into());

    let mut aster = crawled("aster.example", "Aster Observatory", Some(1), 90_000);
    aster.summary = Some("Aster refund information for visitors".into());
    let mut nimbus = crawled("nimbus.example", "Aster refund information", None, 0);
    nimbus.url = Some("https://aster.example/refunds".into());
    nimbus.aliases = vec!["Nimbus Refunds".into()];

    let mut munich = crawled("xn--mnchen-3ya.de", "Stadt München", Some(20_000), 300);
    munich.language = Some("de".into());
    munich.country = Some("DE".into());

    let mut guardian = crawled(
        "theguardian.com",
        "News, sport and opinion from the Guardian's US edition",
        Some(150),
        20_000,
    );
    guardian.aliases = vec!["The Guardian".into()];
    guardian.signals.official_site = true;
    guardian.signals.sitelinks = 120;

    let mut python = crawled("python.org", "Welcome to Python.org", Some(512), 30_000);
    python.aliases = vec!["Python".into()];
    python.headings = vec!["Get Started".into(), "Download".into()];
    python.terms = vec!["python".into(), "download".into()];
    python.kinds = vec!["programming language".into()];
    python.about = Some("general-purpose programming language".into());
    python.intro = Some("Python is a high-level programming language.".into());
    let mut pypi = crawled(
        "pypi.org",
        "PyPI · The Python Package Index",
        Some(2000),
        6000,
    );
    pypi.description = Some("Find, install and publish Python packages".into());

    let mut rakuten = crawled("rakuten.co.jp", "楽天市場", Some(700), 5000);
    rakuten.language = Some("ja".into());
    rakuten.aliases = vec!["Rakuten".into()];

    let mut adult = crawled(
        "freebankvideos.example",
        "Free bank videos xxx",
        Some(3000),
        300,
    );
    adult.description = Some("Videos of banks".into());
    let mut suggestive = crawled("sexybank.example", "Sexy bank videos", Some(3500), 250);
    suggestive.description = Some("Bank videos".into());

    let mut youtube = crawled("youtube.com", "YouTube", Some(2), 100_000);
    youtube.description = Some("Enjoy the videos and music you love".into());
    youtube.signals.official_site = true;

    let mut wells = SiteRecord::new("wellsfargo.com");
    wells.signals.tranco_rank = Some(203);
    wells.link_texts = vec![LinkText::from_linkers("wells fargo", 0b1011)];

    let mut pncbank = crawled("pncbank.com", "PNC Bank", Some(9000), 50);
    pncbank.redirect = Some(Redirect {
        to: "pnc.com".into(),
        at: CRAWLED,
        inner: false,
    });
    let mut pnc = crawled(
        "pnc.com",
        "PNC Bank - Personal & Business Banking",
        Some(800),
        3000,
    );
    pnc.kinds = vec!["bank".into()];
    pnc.country = Some("US".into());

    vec![
        usbank,
        chase,
        america,
        dkb,
        deutsche,
        irs,
        tracker,
        lookalike,
        aster,
        nimbus,
        munich,
        guardian,
        python,
        pypi,
        rakuten,
        adult,
        suggestive,
        youtube,
        wells,
        pncbank,
        pnc,
        crawled("bank.com", "Bank", Some(60_000), 50),
        crawled("example.org", "Example Domain", Some(2000), 500),
    ]
}

/// The searches, each with the settings the page passes on.
fn searches() -> Vec<(&'static str, Options)> {
    let plain = Options::default();
    let us = Options {
        country: Some("US".into()),
        ..Options::default()
    };
    let only_us = Options {
        only_country: true,
        ..us.clone()
    };
    let germany = Options {
        country: Some("DE".into()),
        ..Options::default()
    };
    let german = Options {
        language: Some("de".into()),
        ..Options::default()
    };
    let english = Options {
        language: Some("en".into()),
        ..Options::default()
    };
    let strict = Options {
        safe: SafeSearch::Strict,
        ..Options::default()
    };
    let off = Options {
        safe: SafeSearch::Off,
        ..Options::default()
    };
    vec![
        ("us bank", plain.clone()),
        ("us bank", us.clone()),
        ("us bank", only_us),
        ("us bank login", plain.clone()),
        ("usbank.com", plain.clone()),
        ("chase", us),
        ("bank", germany.clone()),
        ("deutsche bank", german.clone()),
        ("bank of america", plain.clone()),
        ("where can i find chase login", plain.clone()),
        ("tax refund", plain.clone()),
        ("aster refund", plain.clone()),
        ("münchen", germany),
        ("the guardian", plain.clone()),
        ("python download", plain.clone()),
        ("楽天市場", english),
        ("bank videos", plain.clone()),
        ("bank videos", strict),
        ("bank videos", off),
        ("bank site:usbank.com", plain.clone()),
        ("bank -chase", german),
        ("bank video", plain.clone()),
        ("wells fargo", plain.clone()),
        ("pnc bank", plain),
    ]
}

/// The table of `sites`, as a node's index build writes it: best first,
/// each as its stored item.
fn table(dir: &Path, sites: &[SiteRecord]) -> BucketTable {
    let mut items: Vec<_> = sites.iter().map(SiteRecord::to_stored_item).collect();
    items.sort_by(|a, b| {
        site::link_score(b)
            .total_cmp(&site::link_score(a))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut writer = BucketWriter::new_with_budget(&dir.join("buckets"), None).unwrap();
    for item in &items {
        writer.add(item).unwrap();
    }
    writer.finish().unwrap()
}

fn describe(options: &Options) -> String {
    format!(
        "country {}{}, safe {}, language {}",
        options.country.as_deref().unwrap_or("any"),
        if options.only_country { " only" } else { "" },
        options.safe.as_str(),
        options.language.as_deref().unwrap_or("any")
    )
}

fn ranked(out: &mut String, hits: &[plumb_private::Ranked]) {
    for hit in hits {
        writeln!(
            out,
            "{} {:.3} text {:.3} link {:.3} {} {:?} {:?}",
            hit.domain,
            hit.score,
            hit.text_score,
            hit.link_score,
            hit.url,
            hit.title,
            hit.description
        )
        .unwrap();
    }
}

/// A node another node's bucket requests are sealed to.
struct Sealed {
    gateway: Gateway,
    target: Target,
    now: u64,
}

impl Sealed {
    fn new(dir: &Path) -> Sealed {
        let key = plumb_net::load_or_create_key(&dir.join("node.key")).unwrap();
        let now = now_unix();
        let gateway = Gateway::new(&key, now).unwrap();
        let target = Target {
            peer: key.public().to_peer_id().to_string(),
            keys: gateway.keys(),
        };
        Sealed {
            gateway,
            target,
            now,
        }
    }
}

/// A bucket as a node sends it to another: each site as its item.
fn sent(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| site::to_line(&stored::read_item(line.as_bytes()).unwrap()).unwrap())
        .collect()
}

/// A bucket as a node that lies sends it: every site as popular as can
/// be, and its homepage on another site.
fn lying(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            let mut item = stored::read_item(line.as_bytes()).unwrap();
            site::set_signals(
                &mut item,
                &Signals {
                    tranco_rank: Some(1),
                    harmonic_rank: Some(1),
                    pagerank_rank: Some(1),
                    linking_domains: 1_000_000,
                    official_site: true,
                    sitelinks: 300,
                    ..Signals::default()
                },
            );
            item.url = Some("https://login-verify.example/".into());
            site::to_line(&item).unwrap()
        })
        .collect()
}

/// What plumb-private ranks for each search here, as `private_golden.txt`
/// holds it, with each bucket fetched from this node answered by `serve`
/// (its number and stored lines in, the JSON answer out); and every
/// answer served, by bucket.
fn rank_all(serve: impl Fn(u32, &[String]) -> String) -> (String, BTreeMap<u32, String>) {
    let dir = tempfile::tempdir().unwrap();
    let table = table(dir.path(), &sites());
    let node = Sealed::new(dir.path());
    std::fs::create_dir_all(dir.path().join("liar")).unwrap();
    let liar = Sealed::new(&dir.path().join("liar"));
    // What the browser opens of a node's answer of `lines` for `bucket`.
    let ask = |node: &Sealed, bucket: u32, lines: Vec<String>| {
        let (request, opener) = sealed::seal(&node.target, bucket, node.now).unwrap();
        let (asked, reply) = node.gateway.open(&request).unwrap();
        assert!(matches!(asked, Opened::Bucket(asked) if asked.bucket == bucket));
        let records = lines
            .into_iter()
            .map(|record| BucketRecord {
                record,
                proof: None,
                also: Vec::new(),
            })
            .collect();
        let answer = BucketResponse {
            records: Some(records),
            busy: false,
        };
        sealed::open(opener, &seal_response(reply, &answer).unwrap()).unwrap()
    };

    let mut served: BTreeMap<u32, String> = BTreeMap::new();
    let mut ranks = String::from(
        "# What plumb-private ranks of the buckets a node serves and seals:\n\
         # see private_golden.rs before writing it again. Written on records.\n",
    );
    for (query, options) in searches() {
        // What the page does: only the words pick buckets, padded from a
        // fixed secret.
        let lookup = Operators::parse(query).lookup_text();
        let (buckets, keys) = pick_buckets(&lookup, plumb_private::padding(b"golden", &lookup));
        let mut fetched = Vec::new();
        let mut opened = Vec::new();
        for (i, &bucket) in buckets.iter().enumerate() {
            let lines = table.get(bucket).unwrap();
            let json = serve(bucket, &lines);
            fetched.push(plumb_private::read_bucket(&json).unwrap());
            served.insert(bucket, json);
            let (honest, lies) = (sent(&lines), lying(&lines));
            let honest = ask(&node, bucket, honest);
            let lies = ask(&liar, bucket, lies);
            if i % 2 == 0 {
                opened.extend([honest, lies]);
            } else {
                opened.extend([lies, honest]);
            }
        }
        let limit = plumb_private::LIMIT;
        writeln!(ranks, "## {query} ({})", describe(&options)).unwrap();
        writeln!(ranks, "buckets {buckets:?} keys {keys:?}").unwrap();
        let hits = plumb_private::search(query, &keys, fetched, &options, limit);
        ranked(&mut ranks, &hits);
        writeln!(ranks, "# sealed to two nodes, one lying").unwrap();
        let hits = plumb_private::search(query, &keys, opened, &options, limit);
        ranked(&mut ranks, &hits);
    }
    (ranks, served)
}

#[test]
fn private_golden() {
    let (ranks, served) = rank_all(|_, lines| slim_bucket(lines));
    let mut buckets = String::from(
        "# The bytes GET /api/buckets/{table}/{n} answers with for each bucket\n\
         # private_golden.rs fetches: see it before writing this again.\n\
         # Written on items; private_buckets_records.txt holds the records before.\n",
    );
    for (bucket, json) in &served {
        writeln!(buckets, "bucket {bucket} {json}").unwrap();
    }
    let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/web/tests");
    crate::outline::tests::check_golden(&here.join("private_buckets_golden.txt"), &buckets);
    crate::outline::tests::check_golden(&here.join("private_golden.txt"), &ranks);
}

/// A browser holding the buckets as a node answered with them before it
/// moved onto items, trimmed records' JSON, ranks them as it ranks the
/// trimmed items the node answers with now: `private_golden.txt`, line
/// for line.
#[test]
fn buckets_cached_as_records_rank_as_the_golden_file() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/web/tests");
    let cached = std::fs::read_to_string(here.join("private_buckets_records.txt")).unwrap();
    let cached: BTreeMap<u32, String> = cached
        .lines()
        .filter_map(|line| line.strip_prefix("bucket "))
        .map(|line| {
            let (bucket, json) = line.split_once(' ').unwrap();
            (bucket.parse().unwrap(), json.to_string())
        })
        .collect();
    let mut records = cached.values().filter(|json| *json != "[]");
    assert!(records.clone().count() > 0);
    assert!(records.all(|json| json.starts_with("[{\"domain\":")));
    let (ranks, served) = rank_all(|bucket, _| cached[&bucket].clone());
    assert_eq!(served, cached);
    let golden = std::fs::read_to_string(here.join("private_golden.txt")).unwrap();
    assert_eq!(
        ranks.lines().collect::<Vec<_>>(),
        golden.lines().collect::<Vec<_>>()
    );
}
