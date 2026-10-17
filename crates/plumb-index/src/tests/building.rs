//! Building, replacing and opening an index.

use super::*;

fn example_site() -> Item {
    site(
        "example.com",
        Some("Example Domain"),
        None,
        &[],
        &[],
        Signals::default(),
    )
}

#[test]
fn hits_carry_the_site_key_pages() {
    let root = TempDir::new().unwrap();
    let dir = root.path().join("index");
    let mut site = example_site();
    site::set_key_pages(
        &mut site,
        vec![
            KeyPage {
                label: "Docs".into(),
                url: "https://docs.example.com/".into(),
            },
            KeyPage {
                label: "Elsewhere".into(),
                url: "https://other.example/".into(),
            },
        ],
    );
    build_index(&dir, &items(&[site])).unwrap();
    let searcher = Searcher::open(&dir).unwrap();
    let hits = searcher.search("example", 10).unwrap();
    let labels: Vec<&str> = hits[0].key_pages.iter().map(|p| p.label.as_str()).collect();
    assert_eq!(labels, ["Docs"], "only pages on the site itself");
}

/// Makes builds on this thread fail at `step` while it lives.
struct FailAt;

impl FailAt {
    fn step(step: &'static str) -> FailAt {
        FAIL_AT.set(Some(step));
        FailAt
    }
}

impl Drop for FailAt {
    fn drop(&mut self) {
        FAIL_AT.set(None);
    }
}

#[test]
fn rebuilding_replaces_the_old_index() {
    let root = TempDir::new().unwrap();
    let dir = root.path().join("index");
    build_index(&dir, &items(&corpus())).unwrap();
    let old = Searcher::open(&dir).unwrap();
    assert_eq!(old.num_docs(), corpus().len() as u64);

    let stats = build_index(&dir, &items(&[example_site()])).unwrap();
    assert_eq!(stats.docs, 1);
    let new = Searcher::open(&dir).unwrap();
    assert_eq!(new.num_docs(), 1);
    assert!(new.search("us bank", 10).unwrap().is_empty());
    assert_eq!(top(&new, "example"), "example.com");
    assert_eq!(entries(root.path()), ["index"]);
    assert!(entries(&dir).iter().any(|name| name == replace::MARKER));
    // An already open searcher keeps serving the index it opened (on Unix,
    // where deleted files stay readable while open).
    if cfg!(unix) {
        assert_eq!(top(&old, "us bank"), "usbank.com");
    }
}

#[test]
fn failed_builds_keep_the_previous_index() {
    let root = TempDir::new().unwrap();
    let dir = root.path().join("index");
    build_index(&dir, &items(&corpus())).unwrap();

    // Fails after the new index was committed in its staging directory.
    let failing = FailAt::step("after_commit");
    let err = build_index(&dir, &items(&[example_site()])).unwrap_err();
    drop(failing);
    assert!(format!("{err:#}").contains("injected failure"), "{err:#}");

    // The previous index is still there and searchable, with no leftovers.
    let searcher = Searcher::open(&dir).unwrap();
    assert_eq!(searcher.num_docs(), corpus().len() as u64);
    assert_eq!(top(&searcher, "us bank"), "usbank.com");
    assert_eq!(entries(root.path()), ["index"]);

    // The next build goes through.
    build_index(&dir, &items(&[example_site()])).unwrap();
    let searcher = Searcher::open(&dir).unwrap();
    assert_eq!(top(&searcher, "example"), "example.com");
    assert_eq!(entries(root.path()), ["index"]);
}

#[test]
fn refuses_to_replace_a_directory_that_is_not_an_index() {
    let root = TempDir::new().unwrap();
    let data = root.path().join("data");
    fs::create_dir(&data).unwrap();
    fs::write(data.join("notes.txt"), "keep me").unwrap();
    let err = build_index(&data, &items(&corpus())).unwrap_err();
    assert!(err.to_string().contains("refusing"), "{err:#}");
    assert_eq!(
        fs::read_to_string(data.join("notes.txt")).unwrap(),
        "keep me"
    );
    assert_eq!(entries(root.path()), ["data"]);

    // A missing directory is created, an empty one is used.
    let missing = root.path().join("a/b/index");
    assert_eq!(build_index(&missing, &items(&[])).unwrap().docs, 0);
    assert_eq!(Searcher::open(&missing).unwrap().num_docs(), 0);
    let empty = root.path().join("empty");
    fs::create_dir(&empty).unwrap();
    assert_eq!(build_index(&empty, &items(&corpus()[..2])).unwrap().docs, 2);
    assert_eq!(entries(root.path()), ["a", "data", "empty"]);
}

#[test]
fn gone_sites_are_left_out() {
    let dir = TempDir::new().unwrap();
    let mut records = corpus()[..2].to_vec();
    let mut gone = example_site();
    site::make_gone(&mut gone, 1);
    records.push(gone);
    let stats = build_index(dir.path(), &items(&records)).unwrap();
    assert_eq!((stats.docs, stats.skipped), (2, 1));
}

/// The reported case: `plumb index --index myproject` deleted a project
/// folder because it held a file named `meta.json`.
#[test]
fn keeps_a_project_folder_that_holds_a_meta_json() {
    let root = TempDir::new().unwrap();
    // Even a `meta.json` copied from a real index does not make one.
    build_index(&root.path().join("real"), &items(&[example_site()])).unwrap();
    let tantivy_meta = fs::read_to_string(root.path().join("real/meta.json")).unwrap();
    fs::remove_dir_all(root.path().join("real")).unwrap();

    for meta in [
        "{}",
        r#"{"name": "myproject", "version": "1.0.0"}"#,
        &tantivy_meta,
    ] {
        let project = root.path().join("myproject");
        fs::create_dir_all(project.join("src")).unwrap();
        fs::write(project.join("meta.json"), meta).unwrap();
        fs::write(project.join("notes.txt"), "my notes").unwrap();
        fs::write(project.join("src/main.rs"), "fn main() {}").unwrap();

        let err = format!(
            "{:#}",
            build_index(&project, &items(&corpus())).unwrap_err()
        );
        assert!(err.contains("refusing to replace"), "{err}");
        assert!(err.contains("myproject"), "{err}");
        assert_eq!(entries(&project), ["meta.json", "notes.txt", "src"]);
        assert_eq!(fs::read_to_string(project.join("meta.json")).unwrap(), meta);
        assert_eq!(
            fs::read_to_string(project.join("notes.txt")).unwrap(),
            "my notes"
        );
        assert_eq!(
            fs::read_to_string(project.join("src/main.rs")).unwrap(),
            "fn main() {}"
        );
        assert_eq!(entries(root.path()), ["myproject"]);
        fs::remove_dir_all(&project).unwrap();
    }
}

#[test]
fn indexes_built_before_the_marker_are_replaced() {
    let root = TempDir::new().unwrap();
    let dir = root.path().join("index");
    build_index(&dir, &items(&corpus())).unwrap();
    fs::remove_file(dir.join(replace::MARKER)).unwrap();

    build_index(&dir, &items(&[example_site()])).unwrap();
    let searcher = Searcher::open(&dir).unwrap();
    assert_eq!(top(&searcher, "example"), "example.com");
    assert!(entries(&dir).iter().any(|name| name == replace::MARKER));

    // Not when it holds anything besides the index.
    fs::remove_file(dir.join(replace::MARKER)).unwrap();
    fs::write(dir.join("notes.txt"), "keep me").unwrap();
    let err = format!("{:#}", build_index(&dir, &items(&corpus())).unwrap_err());
    assert!(err.contains("it holds notes.txt"), "{err}");
    assert_eq!(Searcher::open(&dir).unwrap().num_docs(), 1);
    assert_eq!(
        fs::read_to_string(dir.join("notes.txt")).unwrap(),
        "keep me"
    );
    assert_eq!(entries(root.path()), ["index"]);
}

#[test]
fn records_without_a_valid_domain_are_skipped() {
    let dir = TempDir::new().unwrap();
    let records = [
        site::new("  "),
        site::new("not a domain"),
        site::new("localhost"),
        site::new("192.168.1.1"),
        site::new("co.uk"),
        site::new("Example.COM"),
    ];
    let stats = build_index(dir.path(), &items(&records)).unwrap();
    let expected = IndexStats {
        docs: 1,
        merged: 0,
        skipped: 5,
        redirected: 0,
    };
    assert_eq!(stats, expected);
    let searcher = Searcher::open(dir.path()).unwrap();
    assert_eq!(searcher.num_docs(), 1);
    assert_eq!(top(&searcher, "example"), "example.com");
}

#[test]
fn records_for_the_same_domain_are_merged() {
    let mut upper = site(
        "Example.com",
        Some("Example Domain"),
        None,
        &["Example"],
        &[("example", 5)],
        obscure(50_000, 10),
    );
    site::set_crawled(&mut upper, Some(100));
    let mut lower = site(
        "www.example.com.",
        Some("Old Example Title"),
        Some("For use in illustrative examples."),
        &["Example Inc"],
        &[("example", 3), ("illustrative examples", 2)],
        popular(900, 20),
    );
    lower.url = Some("https://example.com/".into());
    let unicode = site(
        "münchen.de",
        Some("Landeshauptstadt München"),
        None,
        &[],
        &[],
        obscure(80_000, 30),
    );
    let punycode = site(
        "xn--mnchen-3ya.de",
        None,
        Some("Das offizielle Stadtportal"),
        &[],
        &[("stadtportal", 4)],
        Signals::default(),
    );
    let records = [upper.clone(), unicode, lower.clone(), punycode];
    let dir = TempDir::new().unwrap();
    let stats = build_index(dir.path(), &items(&records)).unwrap();
    let expected = IndexStats {
        docs: 2,
        merged: 2,
        skipped: 0,
        redirected: 0,
    };
    assert_eq!(stats, expected);

    let searcher = Searcher::open(dir.path()).unwrap();
    assert_eq!(searcher.num_docs(), 2);
    let hits = searcher.search("example", 10).unwrap();
    assert_eq!(domains(&hits), ["example.com"]);
    // Merged as `site::merge` does: the crawled site's page fields
    // first, the rest filled in, the best signals of both.
    let mut merged = upper;
    merged.id = "example.com".into();
    lower.id = "example.com".into();
    site::merge(&mut merged, lower);
    assert_eq!(hits[0].title.as_deref(), Some("Example Domain"));
    assert_eq!(
        hits[0].description.as_deref(),
        Some("For use in illustrative examples.")
    );
    assert_eq!(hits[0].url, "https://example.com/");
    assert_eq!(hits[0].link_score, site::link_score(&merged));
    assert_eq!(top(&searcher, "illustrative examples"), "example.com");
    assert_eq!(top(&searcher, "example inc"), "example.com");

    // The Unicode and punycode spellings are one site.
    for query in ["münchen", "munchen", "stadtportal", "münchen.de"] {
        let hits = searcher.search(query, 10).unwrap();
        assert_eq!(domains(&hits), ["xn--mnchen-3ya.de"], "query {query:?}");
        assert_eq!(hits[0].title.as_deref(), Some("Landeshauptstadt München"));
        assert_eq!(
            hits[0].description.as_deref(),
            Some("Das offizielle Stadtportal")
        );
    }
}

#[test]
fn dotted_capital_i_is_found_by_plain_i() {
    // Lowercasing `İ` gives `i` plus a combining dot.
    let mut airport = site::new("istairport.com");
    site::add_link_text(&mut airport, "İstanbul Havalimanı", "havaist.com");
    site::add_link_text(&mut airport, "İstanbul Havalimanı", "turkishairlines.com");
    let mut records = corpus();
    records.push(airport);
    let (_dir, searcher) = build(&records);
    for query in ["istanbul", "İstanbul", "ISTANBUL", "istanbul havalimanı"] {
        assert_eq!(top(&searcher, query), "istairport.com", "query {query:?}");
    }
}

#[test]
fn opening_a_missing_index_fails() {
    let dir = TempDir::new().unwrap();
    assert!(Searcher::open(&dir.path().join("nope")).is_err());
    assert!(Searcher::open(dir.path()).is_err());
}

#[test]
fn empty_index_finds_nothing() {
    let (_dir, searcher) = build(&[]);
    assert_eq!(searcher.num_docs(), 0);
    assert!(searcher.search("us bank", 10).unwrap().is_empty());
}

#[test]
fn unsorted_and_oversized_records_are_tamed() {
    // Sites read from JSON need not respect the caps a clean site keeps.
    let mut record = site::new("example.com");
    record.title = Some(format!("Example {}", "very long title ".repeat(100)));
    site::set_link_texts(
        &mut record,
        &(1..=60)
            .map(|i| LinkText::with_count(format!("anchor {i}"), i))
            .collect::<Vec<_>>(),
    );
    record.names = (100..140).map(|i| format!("alias {i}")).collect();
    let (_dir, searcher) = build(&[record]);
    let hit = &searcher.search("example", 1).unwrap()[0];
    assert!(hit.title.as_deref().unwrap().chars().count() <= MAX_TEXT_CHARS);
    let found = |query: &str| !searcher.search(query, 1).unwrap().is_empty();
    // The 32 most frequent link texts are kept (counts 60 down to 29).
    assert!(found("60") && found("29"));
    assert!(!found("28") && !found("1"));
    // The first 16 aliases are kept.
    assert!(found("100") && found("115"));
    assert!(!found("116"));
}

#[test]
fn searcher_is_shared_between_threads() {
    let (_dir, searcher) = build(&corpus());
    let searcher = Arc::new(searcher);
    let handles: Vec<_> = ["us bank", "bank of america", "chase", "nestle"]
        .into_iter()
        .map(|query| {
            let searcher = Arc::clone(&searcher);
            std::thread::spawn(move || top(&searcher, query))
        })
        .collect();
    let tops: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        tops,
        ["usbank.com", "bankofamerica.com", "chase.com", "nestle.com"]
    );
}

#[test]
fn queries_are_parsed_like_indexed_text() {
    let words = analysis::words_analyzer();
    let joined = analysis::joined_analyzer();
    let parse = |q: &str| ParsedQuery::new(q, &words, &joined);
    assert_eq!(
        parse("U.S. Bank").unwrap(),
        ParsedQuery {
            words: vec!["us".into(), "bank".into()],
            others: vec![None, Some("banks".into())],
            joined: Some("usbank".into()),
            leading: vec![("us".into(), 1), ("usbank".into(), 2)],
            kind: Some("usbank".into()),
            len: 2,
            domain: None,
            terms_boost: 0.0,
            filler_words: false,
            asked: false,
        }
    );
    let keys = |parsed: ParsedQuery| -> Vec<String> {
        parsed.leading.into_iter().map(|(key, _)| key).collect()
    };
    let parsed = parse("bank bank BANK").unwrap();
    assert_eq!(parsed.words, ["bank"]);
    assert_eq!(parsed.joined.as_deref(), Some("bankbankbank"));
    assert_eq!(parsed.len, 3);
    assert_eq!(keys(parsed), ["bank", "bankbank", "bankbankbank"]);
    // Names are compared as indexed: folded, lowercased, punctuation gone.
    assert_eq!(
        keys(parse("Nestlé S.A. login").unwrap()),
        ["nestle", "nestlesa", "nestlesalogin"]
    );
    // A leading "the" may be left out; the words it covers still count it.
    assert_eq!(
        parse("The New York Times").unwrap().leading.last(),
        Some(&("newyorktimes".to_string(), 4))
    );
    assert_eq!(parse("Banks").unwrap().kind.as_deref(), Some("bank"));
    assert_eq!(
        parse("credit unions").unwrap().kind.as_deref(),
        Some("creditunion")
    );
    assert_eq!(
        parse("https://www.usbank.com/").unwrap().domain.as_deref(),
        Some("usbank.com")
    );
    assert_eq!(parse("u.s. bank").unwrap().domain, None);
    assert_eq!(parse("!!!"), None);
    let many = (0..40)
        .map(|i| format!("w{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let many = parse(&many).unwrap();
    assert_eq!(many.words.len(), MAX_QUERY_WORDS);
    assert_eq!(many.leading.len(), MAX_QUERY_WORDS);
    assert_eq!(many.len, 40);
}

#[test]
fn partial_configs_deserialize_with_defaults() {
    let cfg: RankConfig = serde_json_like("alpha", 0.5);
    assert_eq!(cfg.alpha, 0.5);
    assert_eq!(
        cfg,
        RankConfig {
            alpha: 0.5,
            ..RankConfig::default()
        }
    );
    let cfg: RankConfig = serde_json_like("trusted_link_score", 0.3);
    assert_eq!(cfg.trusted_link_score, 0.3);
    assert_eq!(cfg.untrusted_share, RankConfig::default().untrusted_share);
}

#[test]
fn out_of_range_trust_settings_are_tamed() {
    let (_dir, searcher) = build(&lookalike_corpus());
    let expected = searcher.search("us bank login", 10).unwrap();
    for (trusted_link_score, untrusted_share) in [(f32::NAN, f32::NAN), (5.0, 0.5)] {
        let cfg = RankConfig {
            trusted_link_score,
            untrusted_share,
            ..RankConfig::default()
        };
        let hits = searcher.search_with("us bank login", 10, &cfg).unwrap();
        assert_eq!(domains(&hits)[0], "usbank.com");
        assert!(hits.iter().all(|hit| hit.score.is_finite()));
    }
    // NaN falls back to the defaults.
    let nan = RankConfig {
        trusted_link_score: f32::NAN,
        untrusted_share: f32::NAN,
        ..RankConfig::default()
    };
    assert_eq!(
        searcher.search_with("us bank login", 10, &nan).unwrap(),
        expected
    );
}

/// Deserializes a one-field map, without pulling in a JSON crate.
fn serde_json_like(key: &str, value: f32) -> RankConfig {
    use serde::de::value::{Error, MapDeserializer};
    let map = MapDeserializer::<_, Error>::new(std::iter::once((key, value)));
    RankConfig::deserialize(map).unwrap()
}

/// Deterministic xorshift generator for synthetic records.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn word(&mut self) -> &'static str {
        const WORDS: [&str; 48] = [
            "bank", "credit", "union", "home", "loans", "city", "first", "national", "news",
            "shop", "online", "travel", "health", "care", "auto", "parts", "food", "pizza", "best",
            "deals", "tech", "cloud", "data", "music", "games", "sports", "books", "photo", "art",
            "design", "law", "firm", "real", "estate", "school", "college", "church", "garden",
            "pet", "store", "hotel", "golf", "club", "media", "radio", "farm", "energy", "america",
        ];
        WORDS[self.below(WORDS.len() as u64) as usize]
    }
}

fn synthetic_records(n: usize) -> Vec<Item> {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    (0..n)
        .map(|i| {
            let (a, b, c) = (rng.word(), rng.word(), rng.word());
            let mut record = site::new(format!("{a}{b}{i}.com"));
            record.title = Some(format!("{a} {b} {i} | {c} {}", rng.word()));
            record.about = Some((0..12).map(|_| rng.word()).collect::<Vec<_>>().join(" "));
            let link_texts: Vec<LinkText> = (0..rng.below(5))
                .map(|_| {
                    let text = format!("{} {}", rng.word(), rng.word());
                    LinkText::with_count(text, 1 + rng.below(200) as u32)
                })
                .collect();
            site::set_link_texts(&mut record, &link_texts);
            if rng.below(4) == 0 {
                site::add_alias(&mut record, &format!("{a} {b}"));
            }
            let mut signals = site::signals(&record);
            signals.harmonic_rank = Some(1_000 + rng.below(50_000_000));
            signals.linking_domains = rng.below(5_000) as u32;
            site::set_signals(&mut record, &signals);
            record
        })
        .collect()
}

/// About 100k records; slow in debug builds, so run it with
/// `cargo test -p plumb-index --release -- --ignored --nocapture`.
#[test]
#[ignore]
fn builds_100k_records_in_reasonable_time() {
    let mut records = synthetic_records(100_000);
    records.extend(corpus());
    let dir = TempDir::new().unwrap();

    let start = Instant::now();
    let stats = build_index(dir.path(), &items(&records)).unwrap();
    let build_time = start.elapsed();
    assert_eq!(stats.docs, records.len() as u64);

    let searcher = Searcher::open(dir.path()).unwrap();
    assert_eq!(searcher.num_docs(), stats.docs);
    let queries = [
        "us bank",
        "bank of america",
        "bofa",
        "chase",
        "nestle",
        "acme",
    ];
    let expected = [
        "usbank.com",
        "bankofamerica.com",
        "bankofamerica.com",
        "chase.com",
        "nestle.com",
        "acme.com",
    ];
    let start = Instant::now();
    let rounds = 50;
    for _ in 0..rounds {
        for (query, domain) in queries.iter().zip(expected) {
            assert_eq!(top(&searcher, query), domain, "query {query:?}");
        }
    }
    let per_query = start.elapsed() / (rounds * queries.len() as u32);
    eprintln!(
        "built {} docs in {build_time:.2?}; {per_query:.2?} per query",
        stats.docs
    );
    assert!(build_time.as_secs() < 300, "build took {build_time:?}");
}
