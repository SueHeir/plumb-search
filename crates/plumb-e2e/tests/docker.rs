//! The Docker image end to end: a node whose setup is cut short again and
//! again by `docker kill` still ends up searchable, and two nodes on one
//! Docker network find each other, exchange their crawls and answer each
//! other's searches. See the crate docs for how to run these.

use std::time::Duration;

use plumb_e2e::{domains, encode, real, Network, Node};
use serde_json::Value;

const MINUTE: Duration = Duration::from_secs(60);

fn ready(s: &Value) -> bool {
    s["phase"] == "ready"
}

fn idle(s: &Value) -> bool {
    ready(s) && s["step"] == "idle" && s["crawl_left"] == 0
}

/// Fails unless `domain` is among the first `n` of `hits`.
fn assert_in_top(hits: &[String], domain: &str, n: usize, what: &str) {
    assert!(
        hits.iter().take(n).any(|d| d == domain),
        "{what}: {domain} is not in the top {n} of {hits:?}"
    );
}

/// No partial downloads, staging directories or temporary files are left.
fn assert_no_leftovers(node: &Node) {
    let files = node.files();
    let leftovers: Vec<_> = files
        .iter()
        .filter(|f| {
            let name = f.rsplit('/').next().unwrap_or(f);
            name.ends_with(".part") || name.ends_with(".tmp") || name.starts_with(".staging")
        })
        .collect();
    assert!(leftovers.is_empty(), "left over: {leftovers:?}");
}

/// Searches `node`'s `/private` buckets the way the private search page
/// does in a browser, with the page's own Rust code: the node only sees
/// bucket numbers.
fn private_search(node: &Node, query: &str) -> Vec<String> {
    let info = node.get_json("/api/buckets");
    let table = info["table"].as_str().expect("a bucket table");
    let mut next = 0u64;
    let (buckets, keys) = plumb_core::keys::pick_buckets(query, || {
        next += 7919;
        next
    });
    let answers = buckets
        .iter()
        .map(|bucket| {
            let (code, body) = node.get(&format!("/api/buckets/{}/{bucket}", encode(table)));
            assert_eq!(code, 200, "bucket {bucket}: {body}");
            plumb_private::read_bucket(&body).unwrap()
        })
        .collect();
    plumb_private::search(
        query,
        &keys,
        answers,
        &plumb_private::Options::default(),
        plumb_private::LIMIT,
    )
    .into_iter()
    .map(|hit| hit.domain)
    .collect()
}

/// Real data: a node killed while it downloads the seed data, while it
/// downloads the embedding model and while it crawls picks up each time
/// where it was, and ends with the same searchable index, vectors for
/// search by meaning and private search as an uninterrupted one. Without
/// real data, the node starts from the fixtures and is killed while it
/// builds its first index and while it serves.
#[test]
#[ignore = "needs Docker and PLUMB_E2E_IMAGE"]
fn setup_survives_being_killed() {
    let real = real();
    let flags: &[&str] = if real {
        &[
            "--sites",
            "20000",
            "--initial-crawl",
            "300",
            "--no-refresh",
            "--search-by-meaning",
            "--private-search",
        ]
    } else {
        &["--initial-crawl", "0", "--no-refresh", "--private-search"]
    };
    let mut node = Node::new("killed", None, flags);
    if !real {
        node.seed_from_fixtures();
    }

    // Killed during its first step.
    node.start();
    node.wait_for("setup under way", MINUTE, |s| {
        s["step"] == "downloading" || s["step"] == "indexing" || ready(s)
    });
    node.kill();

    // Killed once it can search but is still busy: downloading the rest of
    // the seed data or the model, or crawling.
    node.start();
    let first = node.wait_for("a first index", 20 * MINUTE, ready);
    eprintln!("first index: {first}");
    if real {
        assert!(
            !node.search("wikipedia").is_empty(),
            "the quick index searches"
        );
        node.kill();
        node.start();
        node.wait_for("a crawl under way", 40 * MINUTE, |s| {
            s["step"] == "crawling" && s["crawl_left"].as_u64().unwrap_or(0) > 0
        });
        node.kill();
        let files = node.files();
        eprintln!("files after the kill during the crawl: {files:?}");
    } else {
        node.kill();
    }

    // Left alone, it finishes: crawled, with vectors and, unless it is
    // still downloading the rest of the seed data, idle. When Wikidata
    // answers, that rest (official websites, then their kinds and
    // countries) takes CI longer than this test can wait, so the node may
    // still be at it below; the stop then interrupts that download too.
    node.start();
    let limit = if real { 60 * MINUTE } else { 5 * MINUTE };
    let done = node.wait_for("the end of setup", limit, |s| {
        if !real {
            return idle(s);
        }
        ready(s)
            && s["crawl_left"] == 0
            && s["homepages_visited"].as_u64().unwrap_or(0) > 0
            && s["meaning_sites"].as_u64().unwrap_or(0) > 0
            && (s["step"] == "idle" || s["step"] == "downloading")
    });
    let finished = idle(&done);
    eprintln!("done (finished: {finished}): {done}");
    assert!(done["last_error"].is_null(), "{done}");
    if real {
        assert!(done["sites"].as_u64().unwrap() >= 10_000, "{done}");
        assert!(done["homepages_visited"].as_u64().unwrap() >= 200, "{done}");
        // Wikidata often answers 504 under load. A node does without it,
        // says so and tries again later, which is all this can check then.
        if finished && done["wikidata_missing"] == true {
            assert!(done["wikidata_error"]["retry_at"].is_u64(), "{done}");
            eprintln!("Wikidata was not reached; the node tries again later");
        }
        eprintln!(
            "model files:\n{}",
            node.run_in_volume(
                "sha256sum",
                &[
                    "/data/model/config.json",
                    "/data/model/tokenizer.json",
                    "/data/model/model.safetensors"
                ]
            )
        );
    }

    for (query, domain) in [
        ("wikipedia", "wikipedia.org"),
        ("github", "github.com"),
        ("youtube", "youtube.com"),
    ] {
        let hits = node.search(query);
        eprintln!("{query}: {hits:?}");
        assert_in_top(&hits, domain, 1, query);
        let private = private_search(&node, query);
        eprintln!("private {query}: {private:?}");
        assert_in_top(&private, domain, 1, &format!("private search for {query}"));
    }
    if real {
        // Searches that name no site, answered by meaning. Which sites have
        // text here depends on the 300 homepages crawled and on whether
        // Wikidata answered, so this only checks that they are answered;
        // how good the answers are is the search quality tests' job.
        for (query, domain) in [
            ("free online encyclopedia", "wikipedia.org"),
            ("watch videos online", "youtube.com"),
        ] {
            let hits = node.search(query);
            let rank = hits.iter().position(|d| d == domain).map(|i| i + 1);
            eprintln!("{query}: {domain} at {rank:?} in {hits:?}");
            assert!(!hits.is_empty(), "{query}: no results");
        }
    }
    let (code, _) = node.get("/private");
    assert_eq!(code, 200, "the private search page is built into the image");

    // A clean stop and start keeps it all.
    node.stop();
    if finished {
        assert_no_leftovers(&node);
    } else {
        eprintln!("files after a stop while downloading: {:?}", node.files());
    }
    node.start();
    if finished {
        let again = node.wait_for("a restart", 5 * MINUTE, idle);
        assert_eq!(again["sites"], done["sites"], "{again}");
        assert_eq!(again["index"], done["index"], "a restart rebuilds nothing");
    } else {
        // It searches the index it had and goes back to downloading.
        let again = node.wait_for("a restart", 5 * MINUTE, ready);
        eprintln!("after a restart: {again}");
    }
    assert_in_top(
        &node.search("wikipedia"),
        "wikipedia.org",
        1,
        "after a restart",
    );
}

/// Two nodes on one Docker network: the second is given the first's
/// address, they connect, and each answers the other's network searches.
/// With real data each crawls homepages and publishes them, and the other
/// receives the batches.
#[test]
#[ignore = "needs Docker and PLUMB_E2E_IMAGE"]
fn two_nodes_exchange_crawls_and_searches() {
    let real = real();
    let network = Network::create();
    let common: &[&str] = if real {
        &[
            "--sites",
            "5000",
            "--initial-crawl",
            "400",
            "--no-refresh",
            "--network",
            "--no-upnp",
        ]
    } else {
        &[
            "--initial-crawl",
            "0",
            "--no-refresh",
            "--network",
            "--no-upnp",
        ]
    };
    // Kept off the real network: no default bootstrap or trusted nodes, in
    // images whose `plumb run` has them.
    let help = plumb_e2e::docker(&["run", "--rm", &plumb_e2e::image(), "run", "--help"]);
    let mut common = common.to_vec();
    for flag in ["--no-default-bootstrap", "--no-default-trust"] {
        if help.contains(flag) {
            common.push(flag);
        }
    }
    let common = common.as_slice();
    let mut first = Node::new("first", Some(&network), common);
    if !real {
        first.seed_from_fixtures();
    }
    first.start();
    let status = first.wait_for("a peer id", 20 * MINUTE, |s| {
        s["network"]["peer_id"].is_string()
    });
    let bootstrap = format!(
        "/ip4/{}/tcp/4001/p2p/{}",
        first.ip(),
        status["network"]["peer_id"].as_str().unwrap()
    );

    let mut flags = common.to_vec();
    flags.extend(["--bootstrap", &bootstrap]);
    let mut second = Node::new("second", Some(&network), &flags);
    if !real {
        second.seed_from_fixtures();
    }
    second.start();

    for node in [&first, &second] {
        node.wait_for("a connected peer", 20 * MINUTE, |s| {
            ready(s) && s["network"]["connected_peers"].as_u64().unwrap_or(0) >= 1
        });
    }

    // Each node answers the other's bucket requests.
    for (asker, query, domain) in [
        (&first, "wikipedia", "wikipedia.org"),
        (&second, "github", "github.com"),
    ] {
        let mut answer = Value::Null;
        for _ in 0..30 {
            answer = asker.get_json(&format!("/api/network/search?q={query}"));
            if answer["answered"].as_u64().unwrap_or(0) > 0 && !domains(&answer).is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        eprintln!("{}: network search for {query}: {answer}", asker.name);
        assert!(answer["answered"].as_u64().unwrap_or(0) > 0, "{answer}");
        assert_in_top(&domains(&answer), domain, 1, "network search");
    }

    if real {
        // Each publishes its crawl, and the other receives it.
        for node in [&first, &second] {
            node.wait_for("a published crawl", 40 * MINUTE, |s| {
                s["network"]["batches_published"].as_u64().unwrap_or(0) >= 1
            });
        }
        for node in [&first, &second] {
            let status = node.wait_for("a batch from the other node", 5 * MINUTE, |s| {
                s["network"]["batches_received"].as_u64().unwrap_or(0) >= 1
            });
            eprintln!("{}: {}", node.name, status["network"]);
        }
    }
}
