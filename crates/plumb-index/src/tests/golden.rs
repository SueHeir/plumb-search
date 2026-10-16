//! The site index's golden file, `golden.txt` next to this one: what the
//! index makes of a fixed set of sites. For each site it holds the line a
//! records file of versions up to 0.4 holds of it, the record's own JSON,
//! which nodes still read, its document ([`schema::document`]), the
//! texts the spelling model counts in it ([`schema::spelling_texts`]) and
//! the keys it is filed under in the buckets other nodes take
//! ([`plumb_core::keys::site_keys`]), each made from the site's stored item
//! ([`SiteRecord::to_stored_item`]) as the build reads it. A node's build
//! of the same lines, and the buckets it writes, is checked against a
//! golden file of its own (plumb-node's `outline_golden.txt`).
//!
//! The file was written by the build that read records, before the build
//! moved onto items (and with [`plumb_core::keys::record_keys`]), so it
//! says the two make the same index. It is never
//! written again to make a test pass: a difference is a change in what the
//! index holds, and so in search results. When the index is meant to
//! change, `PLUMB_WRITE_GOLDEN=1 cargo test -p plumb-index golden` writes
//! it, and the change goes through the Batch evals.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use plumb_core::{KeyPage, Redirect};
use tantivy::schema::{OwnedValue, Value as _};

use super::*;
use crate::schema::{self, Fields};

/// The sites of the golden file: those of the other tests, each domain
/// once, then [`rich_sites`], clean as a node's build reads them.
fn golden_sites() -> Vec<SiteRecord> {
    let mut seen = HashSet::new();
    let mut sites = Vec::new();
    let all = [
        corpus(),
        short_names_corpus(),
        lookalike_corpus(),
        typo_corpus(),
        rich_sites(),
    ];
    for mut record in all.into_iter().flatten() {
        record.clean();
        if seen.insert(record.domain.clone()) {
            sites.push(record);
        }
    }
    sites
}

/// Sites that between them use every part of a site a document reads.
fn rich_sites() -> Vec<SiteRecord> {
    let words = |topic: &str| {
        format!(
            "{topic} news every day from reporters around the world with analysis \
             opinion sport culture lifestyle and the stories that matter to readers \
             in every country who want to know what happens next"
        )
    };
    let page = |domain: &str, label: &str, path: &str| KeyPage {
        label: label.into(),
        url: format!("https://www.{domain}/{path}"),
    };

    // A newspaper with every part: a long description, Wikipedia's intro
    // (which a model's summary gives way to), Wikidata's description,
    // kinds and country, headings, terms, page text for a fingerprint,
    // key pages, its own search, and link texts that name it ("The
    // Guardian", by more than enough sites) and ones that do not (an
    // address, a sentence, a text too few sites use).
    let mut guardian = site(
        "theguardian.com",
        Some("News, sport and opinion from the Guardian's global edition | The Guardian"),
        Some(&format!(
            "Latest news, sport, business, comment, analysis and reviews from the \
             Guardian, the world's leading liberal voice. {}",
            "More reporting from around the world. ".repeat(10)
        )),
        &["The Guardian", "Guardian", "The Manchester Guardian"],
        &[
            ("The Guardian", 500),
            ("Guardian", 120),
            ("theguardian.com", 40),
            ("https://www.theguardian.com/", 6),
            ("read the full story on the guardian website today", 9),
            ("Comment is free", 2),
            ("the guardian | world news", 4),
        ],
        Signals {
            harmonic_rank: Some(150),
            pagerank_rank: Some(90),
            tranco_rank: Some(120),
            linking_domains: 95_000,
            official_site: true,
            sitelinks: 80,
            fact_checks: 10,
            fact_agrees: 9,
        },
    );
    guardian.url = Some("https://www.theguardian.com/international".into());
    guardian.intro = Some("The Guardian is a British daily newspaper, founded in 1821.".into());
    guardian.summary = Some("A British newspaper's website.".into());
    guardian.about = Some("British daily newspaper".into());
    guardian.country = Some("gb".into());
    guardian.kinds = vec!["newspaper".into(), "news website".into(), "website".into()];
    guardian.language = Some("en-GB".into());
    guardian.headings = (1..=10).map(|i| format!("Headline section {i}")).collect();
    guardian.terms = vec!["guardian".into(), "news".into(), "opinion".into()];
    guardian.body_text = Some(words("Guardian"));
    guardian.search_url = Some("https://www.theguardian.com/search?q={searchTerms}".into());
    guardian.key_pages = vec![
        page("theguardian.com", "World", "world"),
        page("theguardian.com", "Opinion", "commentisfree"),
        // Not on the site, so not a key page.
        KeyPage {
            label: "Shop".into(),
            url: "https://guardian.example/shop".into(),
        },
    ];
    guardian.links_to = vec!["bbc.co.uk".into()];
    guardian.crawled_at = Some(1_791_605_520);

    // A homepage read on another site's host: its title, description and
    // page text say what that site is, so it goes by its name, and a
    // model's summary stands in for its description.
    let mut borrowed = site(
        "bigbank.example",
        Some("Big Bank Group | Welcome"),
        Some("Big Bank Group, the bank of the group."),
        &["Big Bank"],
        &[("Big Bank", 12)],
        ranked(80_000, 300),
    );
    borrowed.url = Some("https://www.bigbankgroup.example/en/".into());
    borrowed.summary = Some("A bank in Example.".into());
    borrowed.body_text = Some(words("Bank"));
    borrowed.headings = vec!["Accounts".into()];

    // A title that says nothing, so the site goes by its first name; an
    // official site's names key it as its label does, with and without
    // "The".
    let mut acme = site(
        "acmewidgets.example",
        Some("Home"),
        Some("Widgets for every job."),
        &["Acme Widgets", "The Acme Widget Company"],
        &[],
        popular(30_000, 500),
    );
    acme.url = Some("https://acmewidgets.example/".into());
    acme.key_pages = vec![
        page("acmewidgets.example", "Shop", "shop"),
        page("acmewidgets.example", "Log in", "login"),
    ];
    // A title spoiled in decoding, and no names to go by.
    let garbled = site(
        "cafedumonde.example",
        Some("Caf\u{FFFD} du Monde"),
        None,
        &[],
        &[],
        obscure(700_000, 30),
    );

    // No key pages from a crawl: the ones known for it.
    let mut linkedin = site(
        "linkedin.com",
        Some("LinkedIn: Log In or Sign Up"),
        None,
        &["LinkedIn"],
        &[("LinkedIn", 2_000)],
        popular(20, 400_000),
    );
    linkedin.url = Some("https://www.linkedin.com/".into());

    // Adult: by a Wikidata kind, and by the words of a title.
    let mut explicit = site(
        "adultvideos.example",
        Some("Videos"),
        None,
        &[],
        &[],
        obscure(900_000, 40),
    );
    explicit.kinds = vec!["adult website".into()];
    let suggestive = site(
        "lingerieboutique.example",
        Some("Sexy lingerie boutique"),
        Some("Lingerie for every day."),
        &[],
        &[],
        obscure(950_000, 20),
    );

    // Countries: Wikidata's against the domain ending's, and the domain
    // ending's alone. Languages: declared, and as the title is written.
    let mut airfrance = site(
        "airfrance.com",
        Some("Air France - Billets d'avion"),
        Some("Réservez vos billets d'avion."),
        &["Air France"],
        &[("Air France", 300)],
        popular(3_000, 9_000),
    );
    airfrance.country = Some("fr".into());
    airfrance.language = Some("fr".into());
    airfrance.kinds = vec!["airline".into()];
    let mut bundesbank = site(
        "bundesbank.de",
        Some("Deutsche Bundesbank"),
        None,
        &["Deutsche Bundesbank", "Bundesbank"],
        &[],
        popular(9_000, 4_000),
    );
    bundesbank.language = Some("de".into());
    bundesbank.kinds = vec!["central bank".into(), "bank".into()];
    let lenta = site(
        "lenta.ru",
        Some("Лента новостей России и мира сегодня"),
        Some("Новости России и мира"),
        &[],
        &[("Лента", 30)],
        popular(1_500, 20_000),
    );

    // A model's summary alone, and one that a description keeps out.
    let mut summary_only = site(
        "quietsite.example",
        None,
        None,
        &[],
        &[],
        obscure(5_000_000, 3),
    );
    summary_only.summary = Some("A small site about quiet places.".into());
    let mut described = site(
        "describedsite.example",
        Some("Described Site"),
        Some("A site that says what it is."),
        &[],
        &[],
        obscure(5_100_000, 3),
    );
    described.summary = Some("Not used: the site describes itself.".into());

    // A homepage that redirects to another site, which takes its name.
    let mut pnc = site(
        "pnc.com",
        Some("PNC Bank"),
        None,
        &["PNC Financial Services"],
        &[("PNC", 800)],
        popular(1_200, 30_000),
    );
    pnc.url = Some("https://www.pnc.com/en/personal-banking.html".into());
    let mut pncbank = site(
        "pncbank.com",
        None,
        None,
        &[],
        &[("PNC Bank", 40)],
        ranked(90_000, 200),
    );
    pncbank.redirect = Some(Redirect {
        to: "pnc.com".into(),
        at: 1_791_000_000,
        inner: false,
    });
    // To an inner page, which keeps a site of its own.
    let mut inner = site(
        "pncinvest.example",
        Some("PNC Investments"),
        None,
        &[],
        &[],
        obscure(2_000_000, 5),
    );
    inner.redirect = Some(Redirect {
        to: "pnc.com".into(),
        at: 1_791_000_000,
        inner: true,
    });
    inner.crawl_failures = 2;

    vec![
        guardian,
        borrowed,
        acme,
        garbled,
        linkedin,
        explicit,
        suggestive,
        airfrance,
        bundesbank,
        lenta,
        summary_only,
        described,
        pnc,
        pncbank,
        inner,
    ]
}

/// What the golden file holds of `sites`.
fn golden_text(sites: &[SiteRecord]) -> String {
    let schema = schema::schema();
    let fields = Fields::new(&schema).unwrap();
    // Sites redirecting to another of them name it, as when it is built.
    let targets = redirect_targets(
        sites,
        |site| site.domain.as_str(),
        |site| site.redirect.as_ref()?.folds_into(&site.domain),
    );
    let mut redirect_names: HashMap<usize, Vec<String>> = HashMap::new();
    for (i, target) in targets.iter().enumerate() {
        if let Some(target) = target {
            let name = redirect_name(&sites[i].domain);
            redirect_names.entry(*target).or_default().push(name);
        }
    }
    let mut out = String::from(
        "# What the site index makes of the sites of golden.rs: for each, the\n\
         # line a records file holds, the names of the sites redirecting to it,\n\
         # its document's values in the order they are added (a value added\n\
         # several times in a row once, with how many times), the texts the\n\
         # spelling model counts and its bucket keys. Written by\n\
         # `PLUMB_WRITE_GOLDEN=1 cargo test -p plumb-index golden`; see\n\
         # golden.rs before writing it again.\n",
    );
    for (i, record) in sites.iter().enumerate() {
        let names = redirect_names.get(&i).map_or(&[][..], Vec::as_slice);
        let line = json(record);
        writeln!(out, "\nsite {}\nline {line}", record.domain).unwrap();
        if !names.is_empty() {
            writeln!(out, "redirect_names {}", json(names)).unwrap();
        }
        // The index is built from each site's stored item.
        let site = record.to_stored_item();
        let doc = schema::document(&fields, &site, names);
        let mut values: Vec<(String, usize)> = Vec::new();
        for (field, value) in doc.field_values() {
            let value = OwnedValue::from(value.as_value());
            let text = format!("{} {}", schema.get_field_name(field), json(&value));
            match values.last_mut() {
                Some((last, times)) if *last == text => *times += 1,
                _ => values.push((text, 1)),
            }
        }
        for (text, times) in values {
            if times == 1 {
                writeln!(out, "doc {text}").unwrap();
            } else {
                writeln!(out, "doc {text} x{times}").unwrap();
            }
        }
        for text in schema::spelling_texts(&site, names) {
            writeln!(out, "spelling {}", json(&text)).unwrap();
        }
        for key in plumb_core::keys::site_keys(&site) {
            writeln!(out, "key {}", json(&key)).unwrap();
        }
    }
    out
}

/// `value` as JSON on one line, as the golden file writes values.
fn json<T: serde::Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).unwrap()
}

/// Checks `text` against the golden file at `path`, or writes it there
/// when `PLUMB_WRITE_GOLDEN` is set.
fn check_golden(path: &Path, text: &str) {
    if std::env::var_os("PLUMB_WRITE_GOLDEN").is_some() {
        fs::write(path, text).unwrap();
        return;
    }
    let golden =
        fs::read_to_string(path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
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

#[test]
fn the_index_makes_of_each_golden_site_what_the_golden_file_holds() {
    let sites = golden_sites();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tests/golden.txt");
    check_golden(&path, &golden_text(&sites));
}
