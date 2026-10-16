//! Setup and the seed, `plumb ingest` and the sites a node embeds, each
//! checked against a golden file beside this one: what a quick start
//! saves, what folding in the rest of the seed makes of the records
//! (after a quick start and again later), and the changes bringing them up
//! to date writes to the journal (`seed_golden.txt`); what `plumb ingest`
//! writes (`ingest_golden.txt`); and which sites a node and `plumb-tools
//! embed` would embed, and with what text (`embed_golden.txt`). Written
//! by the code that made and read site records, before it made and read
//! sites' items (step 7c of the data format plan), so that the code on
//! items is checked against them byte for byte. Never written again to
//! make a test pass: `PLUMB_WRITE_GOLDEN=1 cargo test -p plumb-node --lib
//! seed_golden` writes them.
//!
//! The seed files are made by hand and differ in what folding them reads:
//! names that are not tidy and the same claim twice, a front page many
//! items claim, one two items of a kind claim and one a far better known
//! item claims, sites on subdomains, a claim misread from an email address
//! and one by a disambiguation page, an international domain name, and
//! more sites than a node keeps. The records they are folded into hold
//! what crawls and earlier folds left: a site twice in the file (once as
//! a record's own JSON), crawl marks, link texts, untidy titles, and names
//! and facts that subdomain sites and misread claims gave other sites.

use std::fmt::Write as _;
use std::io::Write as _;

use plumb_core::{stored, Signals};

use super::*;
use crate::cli::IngestArgs;
use crate::records::{journal_path, load_records, load_sites, Change, RecordStore};

/// The lines of the file at `path`, as they are.
fn lines_of(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Adds `lines` to `out` under the heading `what`.
fn section(out: &mut String, what: &str, lines: &[String]) {
    writeln!(out, "## {what}: {} lines", lines.len()).unwrap();
    for line in lines {
        writeln!(out, "{line}").unwrap();
    }
}

/// The records file at `path` with its journal, loaded as a node loads
/// it, each site as a stored line, best first.
fn loaded(path: &Path) -> Vec<String> {
    let set = load_records(path).unwrap();
    crate::records::sorted_by_link_score(&set)
        .into_iter()
        .map(|record| String::from_utf8(stored::line(record).unwrap()).unwrap())
        .collect()
}

/// Checks `text` against the golden file `name` beside this one, or
/// writes it with `PLUMB_WRITE_GOLDEN` set.
fn check(name: &str, text: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/node/tests")
        .join(name);
    crate::outline::tests::check_golden(&path, text);
}

/// The quick start's Tranco list: more sites than the node keeps, one not
/// written in its canonical form, one that is no registrable domain.
const TRANCO: &str = "1,google.com\n2,wikipedia.org\n3,x.com\n4,ycombinator.com\n\
    5,usbank.com\n6,acme.com\n7,nba.com\n8,quick-only.example\n9,crawled.example\n\
    10,linked.example\n11,example.com\n12,münchen.de\n13,WWW.Shop.Example\n\
    14,stephane.fr\n15,co.uk\n16,firstcu.example\n17,last.example\n";

const CC_RANKS: &str = "#harmonicc_pos\t#harmonicc_val\t#pr_pos\t#pr_val\t#host_rev\t#n_hosts\n\
    1\t1.0\t1\t0.1\tcom.google\t100\n\
    2\t1.0\t3\t0.1\torg.wikipedia\t50\n\
    3\t1.0\t2\t0.1\tcom.ycombinator\t3\n\
    4\t1.0\t5\t0.1\tcom.acme\t2\n\
    5\t1.0\t4\t0.1\tcom.cc-only\t1\n";

/// Wikidata's official websites, with a label that is not tidy and the
/// same claim twice, a front page six items claim (one owns it), one two
/// items claim, one a far better known item claims, sites on subdomains,
/// a claim by a disambiguation page and one misread from an email
/// address, and an international domain name.
fn wikidata() -> String {
    let mut tsv = String::from(
        "item\tlabel\twebsite\n\
         http://www.wikidata.org/entity/Q95\tGoogle\thttps://www.google.com/\n\
         Q9001\t Acme\u{7} Corporation \thttps://www.acme.com/\n\
         Q9001\t Acme\u{7} Corporation \thttps://www.acme.com/\n\
         Q9002\tSte\u{301}phane\thttps://stephane.fr/\n\
         Q52\tWikipedia\thttps://www.wikipedia.org/\n\
         Q9053\tWikipedia community\thttps://www.wikipedia.org/\n\
         Q9031\tX Corp.\thttps://x.com/\n\
         Q9032\tX.com\thttps://x.com/\n\
         Q9010\tNational Basketball Association\thttps://www.nba.com/\n\
         Q9051\tHacker News\thttps://news.ycombinator.com/\n\
         Q9052\tY Combinator\thttps://www.ycombinator.com/\n\
         Q9061\tGoogle Scholar\thttps://scholar.google.com/\n\
         Q9070\tUSB\thttps://www.usbank.com/\n\
         Q9071\tU.S. Bancorp\thttps://www.usbank.com/\n\
         Q9080\tCOE, Moro\thttps://mailto:coe@google.com\n\
         Q9081\tMünchen\thttps://www.münchen.de/\n",
    );
    for i in 1..=5 {
        writeln!(tsv, "Q901{i}\t202{i} NBA season\thttps://www.nba.com/").unwrap();
    }
    tsv
}

const KIND_SITES: &str = "item\tlabel\twebsite\n\
    Q9090\tFirst Credit Union\thttps://www.firstcu.example/\n\
    Q9071\tU.S. Bancorp\thttps://www.usbank.com/\n";

/// The facts of the claims' items, a value a row.
fn facts() -> String {
    let mut tsv = String::from(plumb_ingest::facts::FACTS_HEADER);
    let rows: &[(&str, usize, &str)] = &[
        ("Q95", 0, "US"),
        ("Q95", 1, "search engine"),
        ("Q95", 3, "American search engine"),
        ("Q95", 4, "300"),
        ("Q9001", 0, "US"),
        ("Q9001", 1, "Anvil maker"),
        ("Q9001", 1, "anvil makers"),
        ("Q9001", 1, "business"),
        ("Q9001", 2, "ACME"),
        ("Q9001", 2, "Acme  Corp"),
        ("Q9001", 3, "maker of anvils\u{7}"),
        ("Q9001", 4, "40"),
        ("Q9002", 0, "FR"),
        ("Q52", 1, "online encyclopedia"),
        ("Q52", 4, "300"),
        ("Q9053", 1, "Wikimedia community"),
        ("Q9053", 4, "20"),
        ("Q9031", 0, "US"),
        ("Q9031", 1, "social media company"),
        ("Q9031", 4, "20"),
        ("Q9032", 0, "US"),
        ("Q9032", 1, "online bank"),
        ("Q9032", 4, "20"),
        ("Q9010", 2, "NBA"),
        ("Q9010", 3, "North American basketball league"),
        ("Q9010", 4, "300"),
        ("Q9051", 3, "social news website"),
        ("Q9061", 3, "academic search engine"),
        ("Q9061", 1, "search engine"),
        ("Q9070", 1, "Wikimedia disambiguation page"),
        ("Q9071", 0, "US"),
        ("Q9071", 1, "bank holding company"),
        ("Q9071", 3, "American bank holding company"),
        ("Q9090", 1, "credit union"),
    ];
    for (item, column, value) in rows {
        let before = "\t".repeat(column + 1);
        let after = "\t".repeat(4 - column);
        writeln!(tsv, "{item}{before}{value}{after}").unwrap();
    }
    for i in 1..=5 {
        writeln!(tsv, "Q901{i}\t\t\t\t\t1{i}").unwrap();
    }
    tsv
}

const INTROS: &str = "item\tintro\n\
    Q95\tGoogle is a search engine.\n\
    Q9001\tAcme is a\u{85}company.\n\
    Q9051\tHacker News is a social news website.\n\
    Q9061\tGoogle Scholar is a search engine for scholarly literature.\n";

/// The Common Crawl ranks a node with `config` reads, at the path a
/// download would have put them.
fn cc_ranks_path(config: &NodeConfig) -> PathBuf {
    let url = config.sources.cc_ranks_url.as_deref().unwrap();
    let name = download::cc_domain_ranks_top_file_name(url, config.sites).unwrap();
    config.data_dir.join("seed").join(name)
}

/// A node of `dir`, not started: its steps are run one at a time. The
/// seed files are on disk and new, so nothing is downloaded.
fn node(dir: &Path) -> (Arc<Inner>, tokio::sync::watch::Sender<bool>) {
    let mut config = test_config(dir);
    config.sites = 14;
    config.crawl_home_site = true;
    config.sources.cc_ranks_url = Some(format!(
        "{}/cc-main-2026-domain-ranks.txt.gz",
        closed_port()
    ));
    let seed = dir.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    for (name, text) in [
        (download::TRANCO_FILE_NAME, TRANCO.to_string()),
        (download::WIKIDATA_FILE_NAME, wikidata()),
        (kind_sites::KIND_SITES_FILE_NAME, KIND_SITES.to_string()),
        (plumb_ingest::facts::FACTS_FILE_NAME, facts()),
        (plumb_ingest::intros::INTROS_FILE_NAME, INTROS.to_string()),
    ] {
        std::fs::write(seed.join(name), text).unwrap();
    }
    std::fs::write(cc_ranks_path(&config), CC_RANKS).unwrap();
    let rank = plumb_index::RankConfig::default();
    let opened = open_data_dir(&config, rank).unwrap();
    let (stop, stopped) = tokio::sync::watch::channel(false);
    (
        Arc::new(Inner::new(config, rank, opened, stopped, None)),
        stop,
    )
}

/// A crawled site, as a crawl's change brings it.
fn crawled(domain: &str, title: &str, at: u64) -> SiteRecord {
    let mut record = SiteRecord::new(domain);
    record.url = Some(format!("https://www.{domain}/"));
    record.title = Some(title.into());
    record.crawled_at = Some(at);
    record.crawl_attempted_at = Some(at);
    record.crawl_version = 7;
    record
}

/// What crawls and earlier folds of the seed left in the records of a
/// quick start, as changes in its journal: sites crawled, one only linked
/// to, and names and facts a subdomain site, a misread claim and a
/// disambiguation page's claim gave other sites.
fn crawls() -> Vec<Change> {
    let mut site = crawled("crawled.example", " Crawled\u{7} Site ", 1_791_605_520);
    site.aliases = vec!["Crawled".into(), "Crawled\u{a0}Site".into()];
    site.add_link_text("crawled", "acme.com");
    let mut linked = SiteRecord::new("linked.example");
    linked.add_link_text("Linked", "crawled.example");
    linked.signals.linking_domains = 1;
    let mut google = crawled("google.com", "Google", 1_791_605_600);
    google.aliases = vec!["COE, Moro".into(), "Google".into(), "Google Scholar".into()];
    google.kinds = vec!["college".into(), "search engine".into()];
    google.about = Some("academic search engine".into());
    google.intro = Some("Google Scholar is a search engine for scholarly literature.".into());
    google.country = Some("US".into());
    google.signals.official_site = true;
    google.signals.sitelinks = 300;
    let mut usbank = crawled("usbank.com", "U.S. Bank | Personal Banking", 1_791_605_700);
    usbank.aliases = vec!["USB".into(), "U.S. Bank".into()];
    let mut acme = crawled("acme.com", "Acme\u{7} Anvils ", 1_791_605_800);
    acme.aliases = vec!["Acme\u{301}".into()];
    acme.body_text = Some("Anvils since 1949.".into());
    let mut unreached = SiteRecord::new("example.com");
    unreached.crawl_attempted_at = Some(1_791_605_900);
    unreached.crawl_failures = 2;
    vec![
        Change::Merge { record: site },
        Change::Merge { record: linked },
        Change::Merge { record: google },
        Change::Merge { record: usbank },
        Change::Merge { record: acme },
        Change::Merge { record: unreached },
        Change::Mark {
            domain: "x.com".into(),
            attempted_at: Some(1_791_606_000),
            failures: 1,
        },
    ]
}

/// Appends google.com to the records file again, as a record's own JSON
/// line, as a node before 0.5 wrote it: a file that holds a site twice.
fn add_old_line(path: &Path) {
    let mut google = crawled("google.com", "Google Search", 1_791_000_000);
    google.aliases = vec!["Google Inc.".into()];
    google.headings = vec![" Search ".into()];
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{}", serde_json::to_string(&google).unwrap()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_and_the_seed_make_what_the_golden_file_holds() {
    let dir = tempfile::tempdir().unwrap();
    let (inner, _stop) = node(dir.path());
    let records = inner.paths.records.clone();
    let mut out = String::from(
        "# What a node makes of hand-made seed files: the records file a quick\n\
         # start saves, the records file after the rest of the seed is folded in\n\
         # (after the quick start, and again later), and the changes bringing\n\
         # the records up to date write to the journal. Written by\n\
         # `PLUMB_WRITE_GOLDEN=1 cargo test -p plumb-node --lib seed_golden`.\n",
    );

    // A quick start, from the Tranco list alone.
    super::worker::set_up(&inner).await.unwrap();
    let saved = inner.saved();
    assert!(saved.quick_start && saved.wikidata_missing);
    section(&mut out, "quick start", &lines_of(&records));

    // Crawls before the rest of the seed comes, and a site twice.
    RecordStore::open(&records).save(&crawls()).unwrap();
    add_old_line(&records);
    super::worker::complete_seed(&inner).await.unwrap();
    let saved = inner.saved();
    assert!(!saved.quick_start && !saved.wikidata_missing);
    assert!(!journal_path(&records).exists());
    section(
        &mut out,
        "the seed folded in after the quick start",
        &lines_of(&records),
    );

    // Folded in again, as when Wikidata is asked again, over more crawls.
    let mut again = crawled("example.com", " Example\u{7} Domain ", 1_791_700_000);
    again.aliases = vec!["Example".into(), "Example".into()];
    let mut stephane = crawled("stephane.fr", "Ste\u{301}phane", 1_791_700_100);
    stephane.add_alias("Ste\u{301}phane");
    RecordStore::open(&records)
        .save(&[
            Change::Merge { record: again },
            Change::Merge { record: stephane },
            Change::Gone {
                domain: "linked.example".into(),
                at: 1_791_700_200,
            },
        ])
        .unwrap();
    super::worker::complete_seed(&inner).await.unwrap();
    assert!(!journal_path(&records).exists());
    section(&mut out, "the seed folded in again", &lines_of(&records));

    // The records brought up to date from the seed files, with more claims
    // in them: another subdomain site, another misread claim, and the
    // records as an earlier version left them.
    let wikidata = dir.path().join("seed").join(download::WIKIDATA_FILE_NAME);
    let mut tsv = std::fs::read_to_string(&wikidata).unwrap();
    tsv.push_str("Q9062\tGoogle Maps\thttps://maps.google.com/\n");
    tsv.push_str("Q9082\tAcme Mail\thttps://mailto:info@acme.com\n");
    std::fs::write(&wikidata, tsv).unwrap();
    let mut google = SiteRecord::new("google.com");
    google.aliases = vec!["Google Maps".into(), "COE, Moro".into()];
    google.kinds = vec!["college".into()];
    let mut acme = SiteRecord::new("acme.com");
    acme.aliases = vec!["Acme Mail".into()];
    RecordStore::open(&records)
        .save(&[
            Change::Merge { record: google },
            Change::Merge { record: acme },
        ])
        .unwrap();
    super::worker::refold_seed(&inner).await.unwrap();
    section(
        &mut out,
        "the journal after bringing the records up to date",
        &lines_of(&journal_path(&records)),
    );
    section(&mut out, "the records then", &loaded(&records));
    check("seed_golden.txt", &out);
}

/// `plumb ingest`'s arguments: every seed file of [`node`]'s, the fixtures'
/// crawl, and `records`, written to `out`.
fn ingest_args(seed: &Path, records: Vec<PathBuf>, out: PathBuf) -> IngestArgs {
    let cc_ranks = std::fs::read_dir(seed)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().contains("cc-main"))
        .unwrap();
    IngestArgs {
        tranco: Some(seed.join(download::TRANCO_FILE_NAME)),
        cc_ranks: Some(cc_ranks),
        wat: vec![fixture("sample.wat")],
        wikidata: Some(seed.join(download::WIKIDATA_FILE_NAME)),
        wikidata_facts: Some(seed.join(plumb_ingest::facts::FACTS_FILE_NAME)),
        wikipedia_intros: Some(seed.join(plumb_ingest::intros::INTROS_FILE_NAME)),
        wikidata_kinds: Some(seed.join(kind_sites::KIND_SITES_FILE_NAME)),
        records,
        limit_per_source: None,
        top: None,
        out,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plumb_ingest_writes_what_the_golden_file_holds() {
    let dir = tempfile::tempdir().unwrap();
    let (_inner, _stop) = node(dir.path());
    let seed = dir.path().join("seed");
    let mut out = String::from(
        "# What `plumb ingest` writes of hand-made seed files, the fixtures' crawl\n\
         # and a records file with a journal. Written by\n\
         # `PLUMB_WRITE_GOLDEN=1 cargo test -p plumb-node --lib seed_golden`.\n",
    );

    // A records file of an earlier ingest and crawls: a stale official
    // site, a site twice (once as a record's own JSON), untidy text and a
    // journal.
    let records = dir.path().join("records.jsonl");
    let mut stale = SiteRecord::new("olddomain.com");
    stale.signals = Signals {
        tranco_rank: Some(500),
        harmonic_rank: Some(700),
        pagerank_rank: Some(650),
        official_site: true,
        sitelinks: 12,
        linking_domains: 3,
        ..Signals::default()
    };
    stale.aliases = vec!["Acme Corporation".into()];
    stale.kinds = vec!["company".into()];
    stale.about = Some("old company".into());
    stale.intro = Some("Old is a company.".into());
    stale.country = Some("GB".into());
    stale.title = Some(" Old\u{7} Home ".into());
    stale.add_link_text("acme", "news.example");
    stale.crawled_at = Some(1_700_000_000);
    stale.crawl_attempted_at = Some(1_700_000_100);
    let mut fans = SiteRecord::new("fans.net");
    fans.add_alias("Acme Fans");
    fans.signals.tranco_rank = Some(800);
    let mut usbank = crawled("usbank.com", "U.S. Bank", 1_791_605_520);
    usbank.aliases = vec!["USB".into()];
    let lines = [&stale, &fans, &usbank]
        .map(|record| String::from_utf8(stored::line(record).unwrap()).unwrap());
    std::fs::write(&records, lines.join("\n") + "\n").unwrap();
    add_old_line(&records);
    let mut found = crawled("found.example", "Found\u{301}", 1_791_605_600);
    found.add_link_text("found", "fans.net");
    RecordStore::open(&records)
        .save(&[
            Change::Merge { record: found },
            Change::Mark {
                domain: "fans.net".into(),
                attempted_at: Some(1_791_605_700),
                failures: 3,
            },
        ])
        .unwrap();

    let written = dir.path().join("out.jsonl");
    crate::ingest::run(ingest_args(&seed, vec![records.clone()], written.clone())).unwrap();
    section(&mut out, "everything", &lines_of(&written));

    let best = dir.path().join("best.jsonl");
    crate::ingest::run(IngestArgs {
        limit_per_source: Some(9),
        top: Some(12),
        ..ingest_args(&seed, vec![records.clone()], best.clone())
    })
    .unwrap();
    section(&mut out, "the best 12, 9 of each source", &lines_of(&best));

    // The records alone, refreshed in place.
    crate::ingest::run(IngestArgs {
        tranco: None,
        cc_ranks: None,
        wat: Vec::new(),
        wikidata: None,
        wikidata_facts: None,
        wikipedia_intros: None,
        wikidata_kinds: None,
        ..ingest_args(&seed, vec![records.clone(), written], records.clone())
    })
    .unwrap();
    assert!(!journal_path(&records).exists());
    section(&mut out, "records refreshed in place", &lines_of(&records));
    check("ingest_golden.txt", &out);
}

/// The hash of the text the node embeds of `domain`'s site, as the records
/// file at `path` holds it with its journal.
fn text_hash_of(path: &Path, domain: &str) -> plumb_embed::TextHash {
    let set = load_sites(path).unwrap();
    plumb_embed::text_hash(&plumb_embed::site_text(set.get(domain).unwrap()))
}

/// What the node's embedding holds of each site to embed: its link score,
/// domain, text hash and text.
fn to_embed(todo: &[crate::meaning::ToEmbed]) -> Vec<String> {
    todo.iter()
        .map(|(score, domain, hash, text)| {
            let hash: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
            let text = serde_json::to_string(text).unwrap();
            format!("{score:?}\t{domain}\t{hash}\t{text}")
        })
        .collect()
}

/// The domains `vectors` holds, sorted.
fn held(vectors: &std::sync::RwLock<plumb_embed::Vectors>) -> Vec<String> {
    let mut held = Vec::new();
    vectors.write().unwrap().retain(|domain| {
        held.push(domain.to_string());
        true
    });
    held.sort();
    held
}

/// Sites of a records file to embed, differing in what their text is made
/// of: every part and none, a summary, more link texts than are kept,
/// text longer than is embedded, text that is not tidy, and a gone site.
fn to_embed_records() -> Vec<SiteRecord> {
    let mut tesla = crawled("tesla.com", "Electric  Cars,\tSolar | Tesla", 1_791_605_520);
    tesla.aliases = vec!["Tesla".into(), "Tesla, Inc.".into()];
    tesla.description = Some("tesla".into());
    tesla.about = Some("American electric vehicle company".into());
    tesla.headings = vec!["Model 3".into(), "Model Y".into()];
    tesla.signals.tranco_rank = Some(10);
    tesla.signals.official_site = true;
    let mut cafe = crawled("cafe.fr", "Cafe\u{301} de Flore", 1_791_605_520);
    cafe.description = Some("Le caf\u{e9}\u{7} historique".into());
    cafe.signals.tranco_rank = Some(50);
    let mut summary = SiteRecord::new("summary.org");
    summary.summary = Some("A model's summary of the site.".into());
    summary.signals.official_site = true;
    let mut linked = SiteRecord::new("linked.com");
    for (i, text) in ["one", "two", "three", "four", "five", "six", "seven"]
        .iter()
        .enumerate()
    {
        for linker in 0..(7 - i) {
            linked.add_link_text(text, &format!("l{linker}.example"));
        }
    }
    linked.signals.linking_domains = 7;
    let mut empty = SiteRecord::new("empty.com");
    empty.signals.tranco_rank = Some(1);
    let mut gone = crawled("gone.example", "Gone", 1_700_000_000);
    gone.signals.harmonic_rank = Some(30);
    let mut long = crawled("long.org", "A long page", 1_791_605_520);
    long.body_text = Some(
        (1..=150)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" "),
    );
    let mut untidy = SiteRecord::new("untidy.de");
    untidy.aliases = vec![" Untidy GmbH ".into(), "Untidy\u{a0}GmbH".into()];
    untidy.intro = Some("Untidy GmbH is a\u{85}company.".into());
    untidy.signals.pagerank_rank = Some(70);
    vec![tesla, cafe, summary, linked, empty, gone, long, untidy]
}

/// Writes [`to_embed_records`] to `path`, every other one as a record's
/// own JSON, and crawls since in its journal: a page read again, a site
/// gone, a site new.
fn write_to_embed(path: &Path) {
    let lines: Vec<String> = to_embed_records()
        .iter()
        .enumerate()
        .map(|(i, record)| match i % 2 {
            0 => String::from_utf8(stored::line(record).unwrap()).unwrap(),
            _ => serde_json::to_string(record).unwrap(),
        })
        .collect();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
    let mut cafe = crawled("cafe.fr", "Caf\u{e9} de Flore, Paris", 1_791_700_000);
    cafe.body_text = Some("Ouvert tous les jours.".into());
    let fresh = crawled("new.example", "A new site", 1_791_700_000);
    RecordStore::open(path)
        .save(&[
            Change::Merge { record: cafe },
            Change::Gone {
                domain: "gone.example".into(),
                at: 1_791_700_000,
            },
            Change::Merge { record: fresh },
        ])
        .unwrap();
}

#[test]
fn the_sites_embedded_are_what_the_golden_file_holds() {
    let dir = tempfile::tempdir().unwrap();
    let records = dir.path().join("records.jsonl");
    write_to_embed(&records);
    let mut out = String::from(
        "# The sites a node and `plumb-tools embed` embed of a records file, and\n\
         # the vectors they keep: each site's link score, domain, text hash and\n\
         # text. Written by\n\
         # `PLUMB_WRITE_GOLDEN=1 cargo test -p plumb-node --lib seed_golden`.\n",
    );

    // Vectors of a site whose text is the same now, of sites whose text
    // changed or that have none now, and of a site no longer there.
    let same = text_hash_of(&records, "tesla.com");
    let make = || {
        let mut vectors = plumb_embed::Vectors::new([7; 32], 4);
        vectors.insert("tesla.com", same, &[1, 2, 3, 4]).unwrap();
        for domain in ["cafe.fr", "gone.example", "empty.com", "away.example"] {
            vectors.insert(domain, [0; 32], &[4, 3, 2, 1]).unwrap();
        }
        std::sync::RwLock::new(vectors)
    };

    // plumb-tools embed: the whole file, loaded with its journal.
    for words in [100, 5] {
        let vectors = make();
        let all = crate::meaning::sites_to_embed(&vectors, load_sites(&records).unwrap(), words);
        section(
            &mut out,
            &format!("plumb-tools embed, {words} words"),
            &to_embed(&all),
        );
        section(&mut out, "the vectors kept", &held(&vectors));
    }

    // A node: the sites whose vectors others may give it, and the best to
    // embed, a few at a time and all at once, the journal folded in first.
    let vectors = make();
    let mut wanted: Vec<String> = crate::meaning::wanted_texts(&vectors, &records, 100, None)
        .unwrap()
        .into_iter()
        .map(|(domain, hash)| {
            let hash: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("{domain}\t{hash}")
        })
        .collect();
    wanted.sort();
    section(&mut out, "wanted from other nodes", &wanted);
    assert!(!journal_path(&records).exists());
    section(&mut out, "the records file, folded", &lines_of(&records));
    for (max, words) in [(3, 100), (100, 100), (100, 7)] {
        let vectors = make();
        let (todo, more) =
            crate::meaning::sites_to_embed_from_file(&vectors, &records, max, words, None).unwrap();
        section(
            &mut out,
            &format!("a node, the best {max} at {words} words (more: {more})"),
            &to_embed(&todo),
        );
        section(&mut out, "the vectors kept", &held(&vectors));
    }

    // A file that holds a site twice is read as it is, unfolded.
    write_to_embed(&records);
    add_old_line(&records);
    let mut tesla = crawled("tesla.com", "Tesla", 1_791_000_000);
    tesla.add_alias("Tesla Motors");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&records)
        .unwrap();
    writeln!(
        file,
        "{}",
        String::from_utf8(stored::line(&tesla).unwrap()).unwrap()
    )
    .unwrap();
    let vectors = make();
    let (todo, more) =
        crate::meaning::sites_to_embed_from_file(&vectors, &records, 100, 100, None).unwrap();
    assert!(journal_path(&records).exists());
    section(
        &mut out,
        &format!("a node, a file holding sites twice (more: {more})"),
        &to_embed(&todo),
    );
    section(&mut out, "the vectors kept", &held(&vectors));
    check("embed_golden.txt", &out);
}
