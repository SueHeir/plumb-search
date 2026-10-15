//! The site index's tests: building it, and what a search finds and in
//! what order.

mod brands_and_copies;
mod building;
mod candidates;
mod golden;
mod junk;
mod kinds_and_filters;
mod link_names;
mod meaning;
mod names;
mod ranking;
mod semantic_retention;
mod spelling;
mod subject_evidence;
mod unspaced;

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use plumb_core::{LinkText, Signals, SiteRecord, MAX_TEXT_CHARS};
use plumb_format::Item;
use tempfile::TempDir;

use super::*;
use crate::hits::{brand_label, docs_site_first, without_copies};
use crate::indexing::FAIL_AT;
use crate::names::names_kind;
use crate::query::ParsedQuery;
use crate::rank::NEAREST_RANKED;

/// A record with the given page fields, aliases and link texts.
fn site(
    domain: &str,
    title: Option<&str>,
    description: Option<&str>,
    aliases: &[&str],
    link_texts: &[(&str, u32)],
    signals: Signals,
) -> SiteRecord {
    let mut record = SiteRecord::new(domain);
    record.title = title.map(str::to_string);
    record.description = description.map(str::to_string);
    for alias in aliases {
        record.add_alias(alias);
    }
    for &(text, count) in link_texts {
        let text = plumb_core::normalize_text(text);
        if !text.is_empty() {
            record.link_texts.push(LinkText::with_count(text, count));
        }
    }
    record.signals = signals;
    record
}

/// The default ranking, keeping sites far below a named one.
fn keep_all() -> RankConfig {
    RankConfig {
        named_share: None,
        named_needs_all_words: false,
        ..RankConfig::default()
    }
}

fn popular(tranco_rank: u32, linking_domains: u32) -> Signals {
    Signals {
        tranco_rank: Some(tranco_rank),
        linking_domains,
        official_site: true,
        ..Default::default()
    }
}

fn obscure(harmonic_rank: u64, linking_domains: u32) -> Signals {
    Signals {
        harmonic_rank: Some(harmonic_rank),
        linking_domains,
        ..Default::default()
    }
}

/// Real brands plus decoys that stuff the brand name into their own pages.
fn corpus() -> Vec<SiteRecord> {
    let mut usbank = site(
        "usbank.com",
        Some("U.S. Bank | Personal Banking, Credit Cards, Home Loans & More"),
        Some(
            "Discover U.S. Bank: checking and savings accounts, credit cards, \
             mortgages, auto loans and investing.",
        ),
        &["U.S. Bank", "U.S. Bancorp"],
        &[
            ("US Bank", 1200),
            ("U.S. Bank", 300),
            ("usbank", 300),
            ("US Bank login", 40),
            ("online banking", 25),
        ],
        popular(950, 42_000),
    );
    usbank.url = Some("https://www.usbank.com/".into());

    let mut boa = site(
        "bankofamerica.com",
        Some("Bank of America - Banking, Credit Cards, Loans and Merrill Investing"),
        Some(
            "What would you like the power to do? At Bank of America, our purpose \
             is to help make financial lives better.",
        ),
        &["Bank of America"],
        &[
            ("Bank of America", 2500),
            ("BofA", 300),
            ("bankofamerica.com", 120),
            ("online banking", 80),
        ],
        popular(310, 88_000),
    );
    boa.url = Some("https://www.bankofamerica.com/".into());

    let mut allybank = site("allybank.com", None, None, &[], &[], obscure(2_000_000, 4));
    allybank.url = Some("https://www.ally.com/".into());

    vec![
        usbank,
        site(
            "usbank-login-help.com",
            Some("US Bank Login Help"),
            Some("Step by step help with your US Bank login and US Bank online banking."),
            &[],
            &[],
            Signals::default(),
        ),
        site(
            "usbankreviews.net",
            Some("US Bank Reviews - Is US Bank Good?"),
            Some("Customer reviews of US Bank checking, savings and US Bank credit cards."),
            &["US Bank Reviews"],
            &[("us bank reviews", 3)],
            obscure(9_000_000, 3),
        ),
        boa,
        site(
            "americabank.com",
            Some("America Bank | Community Banking in Texas"),
            Some("America Bank offers personal and business banking."),
            &[],
            &[("America Bank", 6)],
            obscure(4_500_000, 12),
        ),
        site(
            "bankofamerica-fraud-alerts.com",
            Some("Bank of America Fraud Alerts"),
            Some("Report Bank of America phishing and fraud. Bank of America alerts."),
            &[],
            &[],
            Signals::default(),
        ),
        site(
            "chase.com",
            Some("Chase Bank - Credit Cards, Mortgages, Commercial Banking, Auto Loans"),
            None,
            &["Chase"],
            &[("Chase", 3000), ("Chase Bank", 900)],
            popular(120, 120_000),
        ),
        site(
            "ally.com",
            Some("Ally Bank | Online Banking, Savings, Auto Loans & Investing"),
            None,
            &["Ally Bank", "Ally Financial"],
            &[("Ally Bank", 600), ("Ally", 400)],
            popular(2_500, 30_000),
        ),
        allybank,
        site(
            "nestle.com",
            Some("Nestlé Global"),
            Some("Good food, good life."),
            &["Nestlé"],
            &[("Nestlé", 400), ("nestle", 150)],
            popular(4_000, 15_000),
        ),
        site(
            "acme.com",
            Some("Acme Corporation"),
            None,
            &[],
            &[],
            obscure(20_000, 2_000),
        ),
        site(
            "acme.net",
            Some("Acme Corporation"),
            None,
            &[],
            &[],
            obscure(900_000, 20),
        ),
        site(
            "wikipedia.org",
            Some("Wikipedia, the free encyclopedia"),
            None,
            &["Wikipedia"],
            &[("Wikipedia", 9000)],
            popular(10, 1_000_000),
        ),
        site(
            "xn--bcher-kva.de",
            Some("Bücher online kaufen"),
            None,
            &[],
            &[],
            obscure(300_000, 40),
        ),
    ]
}

/// The stored items of `records`, which an index is built from
/// ([`SiteRecord::to_stored_item`]).
fn items(records: &[SiteRecord]) -> Vec<Item> {
    records.iter().map(SiteRecord::to_stored_item).collect()
}

fn build(records: &[SiteRecord]) -> (TempDir, Searcher) {
    let dir = TempDir::new().unwrap();
    let stats = build_index(dir.path(), &items(records)).unwrap();
    assert_eq!(stats.docs, records.len() as u64);
    let searcher = Searcher::open(dir.path()).unwrap();
    (dir, searcher)
}

fn domains(hits: &[Hit]) -> Vec<&str> {
    hits.iter().map(|hit| hit.domain.as_str()).collect()
}

fn top(searcher: &Searcher, query: &str) -> String {
    let hits = searcher.search(query, 10).unwrap();
    assert!(!hits.is_empty(), "no hits for {query:?}");
    hits[0].domain.clone()
}

#[test]
fn a_site_is_found_by_its_page_terms() {
    let mut maker = site(
        "voltmotors.example",
        Some("Volt Motors"),
        None,
        &[],
        &[],
        obscure(200_000, 10),
    );
    maker.terms = ["electric", "electric", "car", "vehicles"]
        .map(String::from)
        .to_vec();
    let other = site(
        "carparts.example",
        Some("Car parts"),
        None,
        &[],
        &[],
        obscure(200_000, 10),
    );
    let (_dir, searcher) = build(&[maker, other]);
    assert_eq!(top(&searcher, "electric vehicles"), "voltmotors.example");
    let off = RankConfig {
        terms_boost: 0.0,
        ..RankConfig::default()
    };
    let hits = searcher.search_with("electric", 10, &off).unwrap();
    assert!(hits.iter().all(|hit| hit.domain != "voltmotors.example"));
}

fn ranked(tranco_rank: u32, linking_domains: u32) -> Signals {
    Signals {
        official_site: false,
        ..popular(tranco_rank, linking_domains)
    }
}

fn with_facts(mut record: SiteRecord, country: Option<&str>, kinds: &[&str]) -> SiteRecord {
    record.country = country.map(str::to_string);
    for kind in kinds {
        record.add_kind(kind);
    }
    record
}

/// A [`Meaning`] with fixed closeness per domain.
struct FixedMeaning(Vec<(&'static str, f32)>);

fn experimental_rank_config() -> RankConfig {
    RankConfig {
        whole_query_relevance: true,
        ..RankConfig::default()
    }
}

fn experimental_search(searcher: &Searcher, query: &str, n: usize) -> Vec<Hit> {
    searcher
        .search_with(query, n, &experimental_rank_config())
        .unwrap()
}

impl Meaning for FixedMeaning {
    fn nearest(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|(domain, _)| domain.to_string())
            .collect()
    }

    fn closeness(&self, domain: &str) -> Option<f32> {
        self.0
            .iter()
            .find(|(d, _)| *d == domain)
            .map(|&(_, closeness)| closeness)
    }
}

/// A [`FixedMeaning`] with `near` the nearest sites and `far` beyond
/// the [`NEAREST_RANKED`] nearest, as a big site far in meaning is.
fn near_and_far(near: &[(&'static str, f32)], far: &[(&'static str, f32)]) -> FixedMeaning {
    let mut all = near.to_vec();
    let pads = ["pad.example"; NEAREST_RANKED];
    all.extend(pads.iter().map(|&pad| (pad, 0.6)));
    all.extend_from_slice(far);
    FixedMeaning(all)
}

/// Short official domains and the spelled-out or one-word domains that
/// beat them before Wikidata names counted as labels.
fn short_names_corpus() -> Vec<SiteRecord> {
    vec![
        site(
            "wsj.com",
            None,
            None,
            &["The Wall Street Journal"],
            &[],
            popular(300, 40_000),
        ),
        site("wall.org", None, None, &[], &[], ranked(20_000, 900)),
        site("wallstreet.com", None, None, &[], &[], ranked(90_000, 200)),
        site(
            "nytimes.com",
            None,
            None,
            &["The New York Times"],
            &[],
            popular(80, 90_000),
        ),
        // Look-alikes spell the whole name out in their domain and
        // title, so they match the words better than the real site.
        site(
            "newyorktimes.com",
            Some("New York Times | New York Times News"),
            Some("New York Times news from New York."),
            &[],
            &[],
            ranked(197_000, 40),
        ),
        site(
            "aa.com",
            None,
            None,
            &["American Airlines"],
            &[],
            popular(900, 12_000),
        ),
        site(
            "americanairlines.com",
            Some("American Airlines | American Airlines flights"),
            None,
            &[],
            &[],
            ranked(506_000, 20),
        ),
        site(
            "americanairlines.fr",
            None,
            None,
            &[],
            &[],
            ranked(60_000, 300),
        ),
    ]
}

fn options(country: &str, only_country: bool) -> SearchOptions {
    SearchOptions {
        country: Some(country.to_string()),
        only_country,
        exact: false,
        ..SearchOptions::default()
    }
}

/// Official sites and the look-alikes built for their brand-plus-intent
/// queries, modeled on `fixtures/`: keyword-stuffed titles, a link from
/// one spam page, no rank.
fn lookalike_corpus() -> Vec<SiteRecord> {
    let spam_linked = || Signals {
        linking_domains: 1,
        ..Signals::default()
    };
    vec![
        site(
            "irs.gov",
            Some("Internal Revenue Service (IRS) | An official website of the United States government"),
            Some("Find tax forms, check your refund status, make a payment and get answers to your tax questions."),
            &["IRS", "Internal Revenue Service"],
            &[("IRS", 2)],
            popular(410, 3),
        ),
        site(
            "irs-tax-refund-help.com",
            Some("IRS Tax Refund Help | Check IRS Refund Status | IRS Online"),
            Some("IRS refund status, IRS tax refund help and IRS online account support."),
            &[],
            &[("IRS Refund", 1)],
            spam_linked(),
        ),
        site(
            "usbank.com",
            Some("Personal and Business Banking | U.S. Bank"),
            None,
            &["U.S. Bank", "U.S. Bancorp"],
            &[("us bank", 2)],
            popular(503, 3),
        ),
        site(
            "usbank-login-help.com",
            Some("US Bank Login Help | US Bank Online Banking Sign In"),
            Some("US Bank login help: sign in to US Bank online banking."),
            &["US Bank Login Help"],
            &[("US Bank Login", 1)],
            spam_linked(),
        ),
        site(
            "amazon.com",
            Some("Amazon.com. Spend less. Smile more."),
            None,
            &["Amazon"],
            &[("Amazon", 2)],
            popular(5, 3),
        ),
        site(
            "amazon-prime-refund.com",
            Some("Amazon Prime Refund | Amazon Account Locked | Amazon Support"),
            Some("Your Amazon Prime refund is ready. Claim your Amazon refund."),
            &[],
            &[],
            Signals::default(),
        ),
        site(
            "chase.com",
            Some("Chase Bank - Credit Cards, Mortgages, Commercial Banking, Auto Loans"),
            None,
            &["Chase", "Chase Bank"],
            &[("Chase", 3)],
            popular(112, 4),
        ),
        site(
            "chasecenter.com",
            Some("Chase Center | San Francisco's Home of the Golden State Warriors"),
            None,
            &["Chase Center"],
            &[("Chase Center", 2)],
            popular(44_016, 2),
        ),
        // Little-known sites: only a link target, or nothing at all.
        site("github.com", None, None, &[], &[], spam_linked()),
        site(
            "maplestreetbakery.com",
            Some("Maple Street Bakery – Fresh Bread Daily"),
            None,
            &[],
            &[],
            Signals::default(),
        ),
        site(
            "panerabread.com",
            Some("Panera Bread | Bakery-Cafe | Order Online"),
            None,
            &["Panera Bread"],
            &[("Panera", 2)],
            popular(2_000, 50),
        ),
    ]
}

/// The names in `dir`, sorted, hidden staging leftovers included.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Well-known sites, an obscure look-alike, and enough sites about
/// pizza and forecasts that the index knows those words.
fn typo_corpus() -> Vec<SiteRecord> {
    let mut records = vec![
        site(
            "google.com",
            Some("Google"),
            None,
            &[],
            &[],
            popular(1, 90_000),
        ),
        site(
            "amazon.com",
            Some("Amazon.com. Spend less. Smile more."),
            None,
            &["Amazon"],
            &[("amazon prime", 300)],
            popular(10, 50_000),
        ),
        site(
            "bankofamerica.com",
            Some("Bank of America - Banking, Credit Cards, Loans"),
            None,
            &["Bank of America"],
            &[],
            popular(300, 20_000),
        ),
        site(
            "weather.com",
            Some("The Weather Channel"),
            None,
            &[],
            &[("weather forecast", 200)],
            popular(150, 30_000),
        ),
        site(
            "piazza.com",
            Some("Piazza: ask questions in class"),
            None,
            &[],
            &[],
            popular(4_000, 3_000),
        ),
        site(
            "gogle.com",
            Some("Gogle - free prizes"),
            None,
            &[],
            &[],
            obscure(40_000_000, 1),
        ),
        site(
            "amazen.com",
            Some("Amazen deals"),
            None,
            &[],
            &[],
            obscure(30_000_000, 2),
        ),
    ];
    records.extend([
        site(
            "hilton.com",
            Some("Hilton"),
            None,
            &[],
            &[],
            popular(900, 15_000),
        ),
        site(
            "hilten.com",
            Some("Hilten"),
            None,
            &[],
            &[],
            ranked(40_000, 2_000),
        ),
        site(
            "capitalone.com",
            Some("Capital One"),
            None,
            &["Capital One"],
            &[],
            popular(200, 20_000),
        ),
        site(
            "tacobell.com",
            Some("Taco Bell"),
            None,
            &["Taco Bell"],
            &[],
            popular(1_500, 10_000),
        ),
        site(
            "fedex.com",
            Some("FedEx"),
            None,
            &[],
            &[],
            popular(400, 20_000),
        ),
        site("edx.org", Some("edX"), None, &[], &[], popular(300, 30_000)),
    ]);
    for i in 0..25 {
        records.push(site(
            &format!("capitol{i}.gov"),
            Some(&format!("Capitol office {i}")),
            None,
            &[],
            &[],
            obscure(3_000_000 + i, 10),
        ));
        records.push(site(
            &format!("capitalnews{i}.com"),
            Some(&format!("Capital city news {i}")),
            None,
            &[],
            &[],
            obscure(6_000_000 + i, 10),
        ));
        records.push(site(
            &format!("taco{i}.com"),
            Some(&format!("Taco stand {i}")),
            None,
            &[],
            &[],
            obscure(4_000_000 + i, 10),
        ));
    }
    for i in 0..25 {
        records.push(site(
            &format!("pizzeria{i}.com"),
            Some(&format!("Pizza place number {i}")),
            None,
            &[],
            &[],
            obscure(1_000_000 + i, 10),
        ));
    }
    for i in 0..20 {
        records.push(site(
            &format!("forecaster{i}.com"),
            Some("Local forecast"),
            None,
            &[],
            &[],
            obscure(2_000_000 + i, 10),
        ));
    }
    records
}

fn suggestion(results: &SearchResults) -> Option<String> {
    results.spelling.as_ref().map(|s| s.query.clone())
}
