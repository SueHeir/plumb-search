//! What a node shows of its page sets, checked against `pages_golden.txt`
//! beside this file. The node holds the fixture sites and pages of every
//! kind of page set: most as each set's articles file, as `plumb-tools
//! fetch-pages` makes them and nodes hand on, and some as the project's
//! item sets, signed and held (books, films, podcasts and Spanish
//! Wikipedia, whose items include a person written before a person's site
//! on a bigger site was left out, and a title not in NFC). The file holds
//! the page index the node builds of them ([`PageSearcher::dump`]) and,
//! for each search, what `/api/search` answers, with and without
//! `full=1`, the results part of the results page, and what the MCP
//! tools answer an AI app (`search`, by kind too, `facts`, `site_info`
//! and `package`). Numbers that are not whole are written to 4 decimal
//! places, as their last bits can differ from one build to the next.
//!
//! Written by the page index that stored each page's JSON, before the
//! index moved onto items (step 11 of the data format plan), so that the
//! index on items is checked against it. Only the answers to `full=1` were
//! written again since, when they came to give each page as its item
//! (step 12), each the same but for its pages. Never written again to make
//! a test pass: `PLUMB_WRITE_GOLDEN=1 cargo test -p plumb-node --lib
//! pages_golden` writes it.

use std::fmt::Write as _;

use plumb_core::facts::{Fact, FactKind};
use plumb_core::profiles::Profile;
use plumb_core::Article;
use plumb_index::pages::Page;

use super::agents::write_set_file;
use super::*;

fn article(title: &str, views: u64, description: &str, item: &str) -> Article {
    Article {
        title: title.into(),
        views,
        description: (!description.is_empty()).then(|| description.into()),
        item: (!item.is_empty()).then(|| item.into()),
        ..Article::default()
    }
}

fn fact(kind: FactKind, value: &str) -> Fact {
    Fact {
        kind,
        value: value.into(),
    }
}

fn profile(service: &str, id: &str) -> Profile {
    Profile {
        service: service.into(),
        id: id.into(),
    }
}

fn strings(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| text.to_string()).collect()
}

/// The articles files the node holds, by set, most read first.
fn set_files() -> Vec<(&'static str, Vec<Article>)> {
    let package = |registry: &str, name: &str, views: u64, description: &str| Article {
        package: Some(plumb_core::packages::PackageInfo {
            registry: registry.into(),
            name: name.into(),
            version: Some("2.32.3".into()),
            released: Some("2026-03-01".into()),
            license: Some("Apache-2.0".into()),
            docs: None,
            repo: Some(format!("https://github.com/example/{name}")),
            homepage: None,
        }),
        ..article(
            name,
            views,
            description,
            &plumb_core::packages::package_item(registry, name),
        )
    };
    let docs: Vec<Article> = plumb_core::article::articles_of(
        include_str!("../../../../plumb-index/tests/fixtures/short-docs.tsv")
            .lines()
            .map(str::to_string),
    )
    .map(|(_, article)| article.unwrap())
    .collect();
    vec![
        (
            "wikipedia-en",
            vec![
                Article {
                    site: Some("apple.com".into()),
                    aliases: strings(&["Apple Computer"]),
                    profiles: vec![profile("x", "Apple")],
                    facts: vec![
                        fact(FactKind::Founded, "1976-04-01"),
                        fact(FactKind::Ceo, "Tim Cook"),
                        fact(FactKind::Headquarters, "Cupertino"),
                    ],
                    lead: Some(
                        "Apple Inc. is an American multinational technology company \
                         headquartered in Cupertino, California."
                            .into(),
                    ),
                    ..article("Apple Inc.", 400_000, "American technology company", "Q312")
                },
                article("Apple", 200_000, "Fruit of the apple tree", "Q89"),
                Article {
                    site: Some("nasa.gov".into()),
                    aliases: strings(&["National Aeronautics and Space Administration"]),
                    facts: vec![fact(FactKind::Founded, "1958-07-29")],
                    ..article("NASA", 150_000, "American space agency", "Q23548")
                },
                Article {
                    site: Some("apple.com".into()),
                    website: Some("https://www.apple.com/leadership/tim-cook/".into()),
                    facts: vec![fact(FactKind::Born, "1960-11-01")],
                    lead: Some(
                        "Timothy Donald Cook (born November 1, 1960) is an American business \
                         executive who is the chief executive officer of Apple Inc."
                            .into(),
                    ),
                    ..article(
                        "Tim Cook",
                        90_000,
                        "American business executive (born 1960)",
                        "Q265852",
                    )
                },
                Article {
                    aliases: strings(&["Madame Curie"]),
                    facts: vec![
                        fact(FactKind::Born, "1867-11-07"),
                        fact(FactKind::Died, "1934-07-04"),
                    ],
                    lead: Some(
                        "Marie Curie was a Polish and naturalised-French physicist and chemist \
                         who conducted pioneering research on radioactivity."
                            .into(),
                    ),
                    names: strings(&["Marie Skłodowska Curie"]),
                    ..article(
                        "Marie Curie",
                        80_000,
                        "Polish and naturalised-French physicist and chemist (1867–1934)",
                        "Q7186",
                    )
                },
                Article {
                    lead: Some(
                        "The Mesozoic Era is an era of Earth's geological history comprising \
                         the Triassic, Jurassic and Cretaceous Periods."
                            .into(),
                    ),
                    ..article("Mesozoic", 40_000, "Era of geological history", "Q23445")
                },
                Article {
                    names: strings(&["Manubrium"]),
                    ..article("Sternum", 30_000, "Long flat bone in the chest", "Q10520")
                },
                Article {
                    site: Some("usbank.com".into()),
                    aliases: strings(&["U.S. Bancorp", "US Bank"]),
                    ..article(
                        "U.S. Bank",
                        12_000,
                        "American bank holding company",
                        "Q2015620",
                    )
                },
                Article {
                    site: Some("chase.com".into()),
                    facts: vec![fact(FactKind::Founded, "1799")],
                    ..article("Chase Bank", 9_000, "American bank", "Q524629")
                },
                article(
                    "Hamstring",
                    8_000,
                    "Muscles at the back of the thigh",
                    "Q215880",
                ),
            ],
        ),
        (
            "github",
            vec![Article {
                site: Some("rust-lang.org".into()),
                ..article(
                    "rust-lang/rust",
                    95_000,
                    "Empowering everyone to build reliable and efficient software.",
                    "",
                )
            }],
        ),
        (
            "stackoverflow",
            vec![Article {
                aliases: strings(&["Undo the last git commit"]),
                ..article(
                    "How do I undo the most recent local commits in Git?",
                    13_000_000,
                    "git, version-control, git-commit, undo",
                    "927358",
                )
            }],
        ),
        (
            "stackexchange",
            vec![article(
                "How do I find which process is using a port?",
                250_000,
                "linux, networking, ports",
                "unix.stackexchange.com/106561",
            )],
        ),
        (
            "music",
            vec![Article {
                aliases: strings(&["Smells Like Teen Spirit Nirvana"]),
                profiles: vec![profile("youtube-video", "hTWKbfoikeg")],
                ..article(
                    "Smells Like Teen Spirit",
                    321_094,
                    "Song by Nirvana, 1991",
                    "recording/5fb524f1-8cc8-4c04-a921-e34c0a911ea7",
                )
            }],
        ),
        ("docs", docs),
        (
            "reference",
            vec![article(
                "Dengue Fever: Symptoms, Causes, and Treatment",
                800,
                "Dengue fever is a mosquito-borne illness: its symptoms, causes and treatment.",
                "https://www.healthline.com/health/dengue-fever",
            )],
        ),
        (
            "subpages2",
            vec![article(
                "Dengue and severe dengue",
                900,
                "WHO fact sheet on dengue: key facts, transmission, symptoms and treatment.",
                "https://www.cdc.gov/dengue/index.html",
            )],
        ),
        (
            "papers",
            vec![Article {
                website: Some("https://arxiv.org/abs/1706.03762".into()),
                paper: Some(plumb_core::papers::PaperMetadata {
                    doi: Some("10.48550/arxiv.1706.03762".into()),
                    arxiv_id: Some("1706.03762".into()),
                    authors: strings(&["Ashish Vaswani", "Noam Shazeer"]),
                    publication_date: Some("2017-06-12".into()),
                    publication_year: Some(2017),
                    venue: Some("Advances in Neural Information Processing Systems".into()),
                    source: "openalex".into(),
                    count_kind: plumb_core::papers::PaperCountKind::Citations,
                    count: 120_000,
                    ..Default::default()
                }),
                ..article(
                    "Attention Is All You Need",
                    120_000,
                    "The dominant sequence transduction models are based on recurrent networks.",
                    "10.48550/arxiv.1706.03762",
                )
            }],
        ),
        (
            "packages",
            vec![
                package("pypi", "requests", 800_000, "Python HTTP for Humans."),
                package(
                    "crates",
                    "serde",
                    700_000,
                    "A generic serialization/deserialization framework",
                ),
            ],
        ),
        (
            "wikidata",
            vec![Article {
                site: Some("linustechtips.com".into()),
                profiles: vec![profile("youtube", "UCXuqSBlHAE6Xw-yeJA0Tunw")],
                aliases: strings(&["LTT"]),
                ..article(
                    "Linus Tech Tips",
                    40,
                    "Canadian technology YouTube channel",
                    "Q30325435",
                )
            }],
        ),
        (
            "wiktionary",
            vec![
                article(
                    "prioritize",
                    200,
                    "(verb) To arrange in order of importance.",
                    "",
                ),
                article("AWOL", 300, "(adjective) Absent without leave.", ""),
            ],
        ),
    ]
}

/// The pages the node holds as the project's item sets, by set: each
/// page as an item set's item gives it back.
fn item_sets() -> Vec<(&'static str, Vec<Page>)> {
    let page = |value: serde_json::Value| -> Page { serde_json::from_value(value).unwrap() };
    vec![
        (
            "books",
            vec![page(serde_json::json!({
                "set": "books", "url": "https://openlibrary.org/works/OL893415W",
                "title": "Dune", "description": "Book by Frank Herbert", "views": 50000,
                "aliases": ["Dune Frank Herbert"],
            }))],
        ),
        (
            "films",
            vec![
                page(serde_json::json!({
                    "set": "films", "url": "https://en.wikipedia.org/wiki/Inception",
                    "title": "Inception", "description": "2010 film by Christopher Nolan",
                    "views": 310, "item": "Q25188", "aliases": ["Inception film"],
                })),
                page(serde_json::json!({
                    "set": "films", "url": "https://www.wikidata.org/wiki/Q1079",
                    "title": "Breaking Bad",
                    "description": "American television series (2008–2013)",
                    "views": 290, "item": "Q1079",
                })),
            ],
        ),
        (
            "podcasts",
            vec![page(serde_json::json!({
                "set": "podcasts", "url": "https://podcastindex.org/podcast/743229",
                "title": "The Daily", "description": "Podcast by The New York Times · News",
                "site": "nytimes.com", "views": 120000, "aliases": ["The Daily podcast"],
                "profiles": [{"service": "spotify-show", "id": "3IM0lmZxpFAY7CwMuv9H4g"}],
            }))],
        ),
        (
            "wikipedia-es",
            vec![
                // Written before a person's site on a bigger site was left
                // out, so its item still says it is about that site.
                page(serde_json::json!({
                    "set": "wikipedia-es", "url": "https://es.wikipedia.org/wiki/Tim_Cook",
                    "title": "Tim Cook", "description": "Empresario estadounidense",
                    "site": "apple.com", "views": 9000, "item": "Q265852",
                    "website": "https://www.apple.com/leadership/tim-cook/",
                    "facts": [{"kind": "born", "value": "1960-11-01"}],
                })),
                page(serde_json::json!({
                    "set": "wikipedia-es", "url": "https://es.wikipedia.org/wiki/Caf%C3%A9",
                    "title": "Cafe\u{301}", "description": "Bebida",
                    "views": 5000, "item": "Q8486",
                })),
            ],
        ),
    ]
}

/// Puts `pages` in `data` as the project's item set of `set`, signed by
/// `publisher`, a page a part.
fn hold_item_set(data: &Path, publisher: &ed25519_dalek::SigningKey, set: &str, pages: &[Page]) {
    let name = format!("plumb/{set}");
    let dir = crate::node::signed_sets::set_dir(data, &name).unwrap();
    let serde_json::Value::Object(manifest) = serde_json::json!({"format": plumb_format::FORMAT,
        "set": name, "generation": "2026-10-12T04:00:00Z", "license": "CC0-1.0"})
    else {
        unreachable!()
    };
    let items = pages.iter().map(plumb_tools::page_items::page_item);
    let fields = plumb_tools::page_items::page_fields();
    let part_ends: Vec<u64> = (1..pages.len() as u64).collect();
    plumb_tools::set::write_set(items, fields, manifest, &dir, &part_ends, 3).unwrap();
    plumb_tools::set::sign_set(&dir, publisher, None, None).unwrap();
}

/// What a node's searches answer about pages, written to the golden file.
const QUERIES: &[&str] = &[
    "apple",
    "apple ceo",
    "apple inc",
    "tim cook",
    "nasa",
    "marie curie",
    "madame curie",
    "triassic jurassic cretaceous",
    "manubrium",
    "us bank",
    "chase",
    "chase bank",
    "hamstring insertion point",
    "rust github",
    "undo last git commit",
    "which process is using a port",
    "smells like teen spirit",
    "python tomllib",
    "tomllib site:docs.python.org",
    "react hooks",
    "dengue fever symptoms",
    "cdc dengue",
    "attention is all you need",
    "requests python",
    "serde crate",
    "linus tech tips",
    "define prioritize",
    "what does awol mean",
    "dune",
    "dune frank herbert",
    "inception",
    "breaking bad",
    "the daily podcast",
    "nytimes",
    "café",
    "marie cruie",
];

/// Calls `tool` on the node's `/mcp` as an AI app on this computer does,
/// waiting out the node's limit on calls: the reply as sent.
async fn mcp_call(addr: SocketAddr, tool: &str, arguments: serde_json::Value) -> String {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    })
    .to_string();
    loop {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        if head.starts_with("HTTP/1.1 429") {
            tokio::time::sleep(Duration::from_millis(1100)).await;
            continue;
        }
        assert!(head.starts_with("HTTP/1.1 200"), "{head}\n{body}");
        return body.to_string();
    }
}

/// `text`, JSON as a node sends it, with each number that is not whole
/// written to 4 decimal places and nothing else changed.
fn rounded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let (mut at, mut in_string) = (0, false);
    while at < bytes.len() {
        let b = bytes[at];
        if in_string {
            let end = if b == b'\\' { at + 2 } else { at + 1 };
            in_string = b != b'"';
            out.extend_from_slice(&bytes[at..end.min(bytes.len())]);
            at = end;
        } else if b == b'-' || b.is_ascii_digit() {
            let start = at;
            while at < bytes.len()
                && matches!(bytes[at], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
            {
                at += 1;
            }
            let number = &text[start..at];
            if number.contains(['.', 'e', 'E']) {
                let x: f64 = number.parse().unwrap();
                let x = serde_json::Number::from_f64((x * 10_000.0).round() / 10_000.0).unwrap();
                out.extend_from_slice(x.to_string().as_bytes());
            } else {
                out.extend_from_slice(number.as_bytes());
            }
        } else {
            in_string = b == b'"';
            out.push(b);
            at += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// The results part of a results page: what is between its first heading
/// and its footer.
fn results_part(html: &str) -> String {
    let start = html.find("<main").unwrap_or(0);
    let end = html[start..]
        .find("<footer")
        .map_or(html.len(), |end| start + end);
    html[start..end].to_string()
}

fn query_param(query: &str) -> String {
    query
        .bytes()
        .map(|b| match b {
            b' ' => "+".to_string(),
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_shows_of_its_page_sets_what_the_golden_file_holds() {
    use ed25519_dalek::SigningKey;
    use plumb_format::PublicKey;

    let dir = seeded_dir();
    let mut pages = 0;
    for (set, mut articles) in set_files() {
        articles.sort_by_key(|article| std::cmp::Reverse(article.views));
        pages += articles.len();
        write_set_file(dir.path(), set, &articles);
    }
    let publisher = SigningKey::from_bytes(&[11; 32]);
    for (set, held) in item_sets() {
        pages += held.len();
        hold_item_set(dir.path(), &publisher, set, &held);
    }
    let mut config = test_config(dir.path());
    config.set_publishers = vec![PublicKey::of(&publisher)];
    config.settings.page_sets = crate::pages::PageSets::parse("wikipedia-es=all").unwrap();
    let node = start(config).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    let searcher = loop {
        let held = node.inner.pages.read().unwrap().clone();
        if let Some((_, searcher)) = held.filter(|(_, s)| s.num_pages() as usize == pages) {
            break searcher;
        }
        assert!(
            Instant::now() < deadline,
            "the page index never held every page"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let mut out = String::new();
    writeln!(out, "# the page index").unwrap();
    out.push_str(&searcher.dump().unwrap());
    for query in QUERIES {
        let q = query_param(query);
        writeln!(out, "# query {query:?}").unwrap();
        let (code, _, body) = get(addr, &format!("/api/search?q={q}")).await;
        assert_eq!(code, 200, "{body}");
        writeln!(out, "api {}", rounded(&body)).unwrap();
        let (code, _, body) = get(addr, &format!("/api/search?q={q}&full=1")).await;
        assert_eq!(code, 200, "{body}");
        writeln!(out, "api full {}", rounded(&body)).unwrap();
        let (code, _, body) = get(addr, &format!("/search?q={q}")).await;
        assert_eq!(code, 200, "{body}");
        writeln!(out, "html {}", results_part(&body)).unwrap();
        let reply = mcp_call(addr, "search", serde_json::json!({ "query": query })).await;
        writeln!(out, "mcp search {}", rounded(&reply)).unwrap();
    }
    for (query, kind) in [
        ("apple", "article"),
        ("git commit", "question"),
        ("requests", "package"),
        ("serde", "package"),
        ("rust", "repo"),
        ("dune", "book"),
        ("attention", "paper"),
        ("tomllib", "docs"),
        ("react hooks", "docs"),
    ] {
        let reply = mcp_call(
            addr,
            "search",
            serde_json::json!({ "query": query, "kind": kind }),
        )
        .await;
        writeln!(out, "# mcp search {query:?} kind {kind}").unwrap();
        writeln!(out, "{}", rounded(&reply)).unwrap();
    }
    for (tool, arguments) in [
        ("facts", serde_json::json!({ "subject": "apple inc" })),
        ("facts", serde_json::json!({ "subject": "tim cook" })),
        ("facts", serde_json::json!({ "subject": "nasa" })),
        ("site_info", serde_json::json!({ "domain": "apple.com" })),
        ("site_info", serde_json::json!({ "domain": "nasa.gov" })),
        ("site_info", serde_json::json!({ "domain": "chase.com" })),
        ("package", serde_json::json!({ "name": "requests" })),
        (
            "package",
            serde_json::json!({ "name": "serde", "registry": "crates" }),
        ),
    ] {
        let reply = mcp_call(addr, tool, arguments.clone()).await;
        writeln!(out, "# mcp {tool} {arguments}").unwrap();
        writeln!(out, "{}", rounded(&reply)).unwrap();
    }
    for (path, what) in [
        ("/api/search?q=attention&kind=paper", "papers"),
        (
            "/api/search?q=transformers&kind=paper&after=2016",
            "papers after",
        ),
        (
            "/api/search?q=attention+is+all+you+need&kind=paper&full=1",
            "papers full",
        ),
    ] {
        let (code, _, body) = get(addr, path).await;
        writeln!(out, "# {what} {code}").unwrap();
        writeln!(out, "{}", rounded(&body)).unwrap();
    }
    node.shutdown().await.unwrap();

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/node/tests/pages_golden.txt");
    if std::env::var_os("PLUMB_WRITE_GOLDEN").is_some() {
        std::fs::write(&path, &out).unwrap();
        return;
    }
    let golden = std::fs::read_to_string(&path).unwrap();
    for (n, (want, got)) in golden.lines().zip(out.lines()).enumerate() {
        assert_eq!(want, got, "{}:{} differs", path.display(), n + 1);
    }
    assert_eq!(
        golden.lines().count(),
        out.lines().count(),
        "{}",
        path.display()
    );
}
