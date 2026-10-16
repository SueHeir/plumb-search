//! What a node takes in of other nodes' sites, checked against
//! `network_golden.txt` beside this file: what it keeps of a batch of
//! crawls another node published, in its inbox and its headlines; what it
//! keeps and ranks of the sites a network search found; what it keeps of
//! a page of a trusted node's list when filling its space, and makes of
//! the pages it sets up from; what it publishes of the crawls in a records
//! file; and what folding the inbox in leaves in its records and icons.
//! The plumb-net side, what is sent and what a node takes from it, is
//! plumb-net's `node/tests/golden.txt`.
//!
//! Written on records, before the network moved onto sites' items (step 9
//! of the data format plan), so that the network on items is checked
//! against it byte for byte. Never written again to make a test pass:
//! `PLUMB_WRITE_GOLDEN=1 cargo test -p plumb-node --lib network_golden`
//! writes it.

use std::fmt::Write as _;

use base64::Engine as _;
use plumb_core::{stored, Headline, KeyPage, LinkText, RecordSet};
use plumb_index::{RankConfig, SearchOptions};
use plumb_net::{FoundSite, NetSearch};

use super::*;
use crate::web::{rank_found, take_found};

/// When the sites here were crawled, fixed so the lines come out the same.
const CRAWLED: u64 = 1_792_000_000;

/// The stored line the inbox and the records file get of `record`.
fn line(record: &SiteRecord) -> String {
    String::from_utf8(stored::line(record).unwrap()).unwrap()
}

/// The lines of the file at `path` from line `from` on, and how many it
/// has in all.
fn lines_from(path: &Path, from: usize) -> (Vec<String>, usize) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let n = lines.len();
    (lines.into_iter().skip(from).collect(), n)
}

/// Adds the inbox lines past the `seen` already added under `what`.
fn inbox(out: &mut String, inner: &Inner, what: &str, seen: &mut usize) {
    let (lines, n) = lines_from(&inner.paths.inbox, *seen);
    writeln!(out, "## {what}: {} inbox lines", lines.len()).unwrap();
    for line in lines {
        writeln!(out, "{line}").unwrap();
    }
    *seen = n;
}

fn crawl(domain: &str, title: &str) -> SiteRecord {
    let mut record = SiteRecord::new(domain);
    record.url = Some(format!("https://www.{domain}/"));
    record.title = Some(title.into());
    record.description = Some(format!("All about {domain}."));
    record.crawled_at = Some(CRAWLED);
    record
}

fn deny(target: &str) -> plumb_tools::set::DenyEntryArgs {
    plumb_tools::set::DenyEntryArgs {
        category: "copyright".into(),
        target: Some(target.into()),
        hash_of: None,
        any_query: false,
        places: Vec::new(),
        id: None,
    }
}

/// A valid icon, as another node shares it: base64 PNG.
fn icon() -> String {
    let png = plumb_crawl::normalize_icon(&crate::icons::tests::bmp()).unwrap();
    base64::engine::general_purpose::STANDARD.encode(png)
}

/// Crawls another node published, as plumb-net hands them to the node
/// ([`plumb_net::node::RecordDelivery`]): sites the node holds, one with a
/// key page and one with a headline its denylist takes off, a site it
/// denies, sites it does not hold, a site only linked to, and feed checks.
fn delivery(now: u64) -> Vec<SiteRecord> {
    let mut chase = crawl("chase.com", "Chase Bank");
    chase.headings = vec!["Checking".into(), "Savings".into()];
    chase.body_text = Some("Bank with Chase. ".repeat(30));
    chase.terms = vec!["bank".into(), "checking".into()];
    chase.key_pages = vec![
        KeyPage {
            label: "Savings".into(),
            url: "https://www.chase.com/savings".into(),
        },
        KeyPage {
            label: "Denied".into(),
            url: "https://www.chase.com/denied".into(),
        },
    ];
    chase.link_texts = vec![LinkText::from_linkers(
        "chase",
        plumb_core::linker_bit("a.org"),
    )];
    chase.signals.linking_domains = 3;
    chase.icon = Some(icon());
    let mut reddit = crawl("reddit.com", "Reddit");
    reddit.aliases = vec!["Reddit".into(), "reddit".into()];
    let headline = |title: &str, url: &str| Headline {
        title: title.into(),
        url: url.into(),
        at: now - 600,
    };
    let mut reddit_news = SiteRecord::new("reddit.com");
    reddit_news.news = vec![headline("Front page", "https://www.reddit.com/r/news/1")];
    let mut bbc_news = SiteRecord::new("bbc.com");
    bbc_news.news = vec![
        headline("Big news", "https://www.bbc.com/news/1"),
        headline("Denied news", "https://www.bbc.com/news/denied"),
    ];
    let denied = crawl("denied.example", "Denied");
    let new_bank = crawl("new-savings-bank.example", "New Savings Bank");
    let mut linked = SiteRecord::new("wellsfargo.com");
    linked.link_texts = vec![LinkText::from_linkers(
        "wells fargo",
        plumb_core::linker_bit("chase.com"),
    )];
    linked.signals.linking_domains = 1;
    vec![
        chase,
        reddit_news,
        denied,
        new_bank,
        reddit,
        linked,
        bbc_news,
    ]
}

/// A found site, as a network search hands it to the node: the site as
/// answered, and the signed crawl when one counts.
fn found(
    record: SiteRecord,
    shared: Option<SiteRecord>,
    trusted: bool,
    confirmed: bool,
) -> FoundSite {
    let crawler = "12D3KooWK99VoVxNE7XzyBwXEzW7xhK7Gpv85r9F3V3fyKSUKPH5".to_string();
    FoundSite {
        record,
        verified: shared.is_some(),
        crawler: shared.as_ref().map(|_| crawler.clone()),
        answers: 1,
        crawlers: shared.iter().map(|_| crawler.clone()).collect(),
        confirmed,
        trusted,
        shared,
        proof: None,
    }
}

/// `json` with every fraction in it rounded to 3 decimal places. The
/// scores of a ranking can differ in their last bit from one run to the
/// next, so they are pinned to 3 places, none of them near the edge of
/// one, with the order they put the sites in.
fn rounded(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    let (mut quoted, mut escaped) = (false, false);
    let mut chars = json.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted || !(c == '-' || c.is_ascii_digit()) {
            out.push(c);
            (quoted, escaped) = match (quoted, escaped, c) {
                (true, false, '\\') => (true, true),
                (true, false, '"') | (false, _, '"') => (!quoted, false),
                _ => (quoted, false),
            };
            continue;
        }
        let mut number = c.to_string();
        while let Some(&c) = chars.peek().filter(|c| "0123456789.eE+-".contains(**c)) {
            number.push(c);
            chars.next();
        }
        match number.parse::<f64>() {
            Ok(x) if number.contains(['.', 'e', 'E']) => {
                let x = serde_json::Number::from_f64((x * 1000.0).round() / 1000.0);
                out.push_str(&x.map_or(number, |x| x.to_string()));
            }
            _ => out.push_str(&number),
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_node_takes_in_the_golden_sites() {
    let dir = seeded_dir();
    let publisher = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
    hold_denylist(
        dir.path(),
        &publisher,
        &[
            deny("site:denied.example"),
            deny("url:https://www.chase.com/denied"),
            deny("url:https://www.bbc.com/news/denied"),
        ],
    );
    let mut config = test_config(dir.path());
    config.set_publishers = vec![plumb_format::PublicKey::of(&publisher)];
    config.focus_topics = vec!["savings bank".into()];
    let node = start(config).await.unwrap();
    wait_for(node.addr(), "initial index", ready_and_idle).await;
    let inner = node.inner.clone();
    // Made when the node joins the network, which it does not here.
    std::fs::create_dir_all(&inner.paths.net).unwrap();
    let mut out = String::from(
        "# What a node takes in of other nodes' sites: see network_golden.rs\n\
         # before writing it again. Written on records.\n",
    );
    let mut seen = 0;

    // A batch of crawls another node published.
    // As plumb-net hands them over: the stored items of the sites.
    let (crawls, news) = network::delivered(&inner, listed(&delivery(now_unix())));
    network::persist_delivery(inner.clone(), crawls, news, None, None).await;
    inbox(&mut out, &inner, "a published batch", &mut seen);
    let headlines: serde_json::Value =
        serde_json::from_slice(&std::fs::read(inner.paths.news.join("headlines.json")).unwrap())
            .unwrap();
    for (domain, kept) in headlines.as_object().unwrap() {
        for headline in kept.as_array().unwrap() {
            writeln!(
                out,
                "headline {domain} {} {}",
                headline["title"], headline["url"]
            )
            .unwrap();
        }
    }
    let languages: serde_json::Value =
        serde_json::from_slice(&std::fs::read(inner.paths.news.join("languages.json")).unwrap())
            .unwrap();
    writeln!(out, "languages {languages}").unwrap();

    // What a network search found: what the node keeps, then ranks.
    let mut chase = crawl("chase.com", "Chase Bank, signed");
    chase.key_pages = vec![
        KeyPage {
            label: "Open a savings account".into(),
            url: "https://www.chase.com/savings/open".into(),
        },
        KeyPage {
            label: "Denied".into(),
            url: "https://www.chase.com/denied".into(),
        },
    ];
    chase.body_text = Some("Savings accounts at Chase bank. ".repeat(10));
    let mut answered = chase.clone();
    answered.signals.tranco_rank = Some(17);
    let mut usbank = crawl("usbank.com", "U.S. Bank savings");
    usbank.crawled_at = Some(CRAWLED - 60);
    let mut ranked = SiteRecord::new("ranked-savings-bank.example");
    ranked.aliases = vec!["Ranked Savings Bank".into()];
    ranked.signals.tranco_rank = Some(900);
    let new_bank = crawl("new-savings-bank.example", "New Savings Bank");
    let reddit = crawl("reddit.com", "Reddit savings bank thread");
    let mut search = NetSearch {
        buckets: 4,
        asked: 4,
        answered: 3,
        found: vec![
            found(answered, Some(chase), true, false),
            found(usbank.clone(), Some(usbank), false, true),
            found(
                crawl("denied.example", "Denied savings bank"),
                None,
                false,
                false,
            ),
            found(new_bank.clone(), Some(new_bank), false, true),
            found(reddit.clone(), Some(reddit), false, false),
            found(ranked, None, false, false),
        ],
        ..NetSearch::default()
    };
    take_found(&*inner, &mut search);
    inbox(&mut out, &inner, "a network search", &mut seen);
    let kept: Vec<SiteRecord> = search
        .found
        .iter()
        .filter_map(|f| f.keeps().cloned())
        .collect();
    network::keep_found(&inner, kept);
    inbox(&mut out, &inner, "the same search again", &mut seen);
    let ranked = rank_found(
        search,
        "savings bank",
        10,
        &RankConfig::default(),
        &SearchOptions::default(),
    )
    .unwrap();
    let ranked = rounded(&serde_json::to_string(&ranked).unwrap());
    writeln!(out, "ranked {ranked}").unwrap();

    // A page of a trusted node's list, kept when filling, and only what is
    // about the node's topics once it keeps only that.
    let filled = || {
        let mut bank = crawl("savings-bank.example", "Savings Bank");
        bank.description = Some("A savings bank".into());
        let mut other = crawl("cooking.example", "Cooking");
        other.link_texts = vec![LinkText::with_count("recipes", 2)];
        vec![bank, other, crawl("denied.example", "Denied")]
    };
    for (what, topics) in [
        ("a page filled", None),
        ("a page filled about the topics", Some(inner.keep_topics())),
    ] {
        let mut records = filled();
        super::fill::keep_filled(&inner, &mut records, topics.as_ref());
        network::append_inbox(&inner, &listed(&records)).unwrap();
        inbox(&mut out, &inner, what, &mut seen);
    }

    // The pages a node sets up from, as the records file it saves.
    let mut set = RecordSet::new();
    let mut first = crawl("WWW.Setup.Example", " Setup\u{7} Site ");
    first.aliases = vec!["Setup".into(), "Setup".into()];
    let mut ranked = SiteRecord::new("setup-ranked.example");
    ranked.signals.tranco_rank = Some(3);
    let mut again = SiteRecord::new("setup.example");
    again.signals.tranco_rank = Some(40);
    again.link_texts = vec![LinkText::with_count("setup", 4)];
    let mut bot = crawl("bot-check.example", "Just a moment...");
    bot.description = None;
    bot.signals.tranco_rank = Some(5);
    super::fill::take_seed_page(
        &inner,
        &mut set,
        vec![first, ranked, crawl("denied.example", "Denied")],
    );
    super::fill::take_seed_page(&inner, &mut set, vec![again, bot]);
    let seed: Vec<String> = set.into_sorted_vec().iter().map(line).collect();
    writeln!(out, "## set up from: {} sites", seed.len()).unwrap();
    for line in seed {
        writeln!(out, "{line}").unwrap();
    }

    // What the node publishes of the crawls in a records file.
    let published = dir.path().join("published.jsonl");
    let mut python = crawl("python.org", " Welcome\u{7} to Python.org ");
    python.body_text = Some("Python lets you work quickly. ".repeat(60));
    python.key_pages = vec![
        KeyPage {
            label: "Downloads".into(),
            url: "https://www.python.org/downloads/".into(),
        },
        KeyPage {
            label: "Denied".into(),
            url: "https://www.chase.com/denied".into(),
        },
    ];
    python.links_to = vec!["pypi.org".into()];
    python.signals.tranco_rank = Some(512);
    python.crawl_attempted_at = Some(CRAWLED + 60);
    python.crawl_failures = 1;
    python.icon = Some(icon());
    let mut older = crawl("older.example", "Older");
    older.crawled_at = Some(CRAWLED - 100);
    let mut too_old = crawl("too-old.example", "Too old");
    too_old.crawled_at = Some(CRAWLED - 1_000);
    let mut file = Vec::new();
    for record in [
        python,
        SiteRecord::new("ranked.example"),
        crawl("denied.example", "Denied"),
        older,
        too_old,
    ] {
        file.extend(stored::line(&record).unwrap());
        file.push(b'\n');
    }
    std::fs::write(&published, file).unwrap();
    let to_publish =
        network::to_publish(&inner, &published, CRAWLED - 1_000, CRAWLED - 500).unwrap();
    writeln!(out, "## published: {} sites", to_publish.len()).unwrap();
    for (site, inbox) in to_publish.iter().zip(to_publish.lines()) {
        writeln!(out, "signed {}", plumb_core::site::to_line(&site).unwrap()).unwrap();
        writeln!(out, "inbox {}", String::from_utf8_lossy(inbox)).unwrap();
    }

    // What folding the inbox in leaves in the records and icons.
    let absorbed = network::absorb_inbox(&inner).unwrap();
    writeln!(out, "## absorbed {absorbed}").unwrap();
    let touched = [
        "bbc.com",
        "chase.com",
        "cooking.example",
        "denied.example",
        "new-savings-bank.example",
        "reddit.com",
        "savings-bank.example",
        "usbank.com",
        "wellsfargo.com",
    ];
    let sites = crate::records::load_sites(&inner.paths.records).unwrap();
    for line in sites.into_sorted().lines() {
        let line = String::from_utf8(line.to_vec()).unwrap();
        let site = stored::read_item(line.as_bytes()).unwrap();
        if touched.contains(&site.id.as_str()) {
            writeln!(out, "{line}").unwrap();
        }
    }
    let icons = crate::icons::IconStore::new(&inner.paths.icons);
    for domain in touched {
        if let Some(icon) = icons.get(domain) {
            writeln!(out, "icon {domain} {} bytes", icon.len()).unwrap();
        }
    }
    node.shutdown().await.unwrap();

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/node/tests/network_golden.txt");
    crate::outline::tests::check_golden(&path, &out);
}
