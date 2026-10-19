//! The tools that change what a node shows, and the `improve_search`
//! prompt.

use std::sync::Mutex;

use crate::mcp::manage::{MAX_FOCUS_TOPICS, TOOLS};
use crate::node::{Fetched, NodeSettings, Status, Tuning};
use crate::wanted_pages::WantedSite;
use plumb_crawl::WantedPage;

use super::*;

/// A node that keeps its settings and notes what it was asked to fetch.
#[derive(Default)]
struct Owned {
    settings: Mutex<NodeSettings>,
    changes: Mutex<usize>,
    fetched: Mutex<Vec<(String, Vec<String>)>>,
}

impl StatusSource for Owned {
    fn status(&self) -> Status {
        unreachable!("the tools never ask for the node's status")
    }

    fn settings(&self) -> Option<NodeSettings> {
        Some(self.settings.lock().unwrap().clone())
    }

    fn change_settings(&self, settings: NodeSettings) -> Result<()> {
        *self.settings.lock().unwrap() = settings;
        *self.changes.lock().unwrap() += 1;
        Ok(())
    }

    fn fetch_pages(&self, start: &str, words: Vec<String>) -> Result<Fetched> {
        self.fetched
            .lock()
            .unwrap()
            .push((start.to_string(), words.clone()));
        Ok(Fetched {
            site: "nasa.gov".into(),
            words,
            fetched: 9,
            new: 1,
            pages: vec![WantedPage {
                url: "https://science.nasa.gov/exoplanets/".into(),
                title: "Exoplanets".into(),
                depth: 1,
                ..Default::default()
            }],
        })
    }

    fn tuning(&self) -> Option<Tuning> {
        Some(Tuning {
            focus_topics: self.settings().unwrap().focus_topics,
            fixed_focus_topics: Vec::new(),
            crawls_paused: Some("Background updates are off".into()),
            shows_pages: true,
            asked_sites: vec![WantedSite {
                site: "nasa.gov".into(),
                wanted_at: plumb_core::now_unix(),
                looked_at: plumb_core::now_unix(),
                words: vec!["discoveries exoplanet".into()],
                pages: 3,
            }],
            asked_pages: 3,
        })
    }
}

fn managed(node: &Arc<Owned>) -> Mcp {
    server(Vec::new())
        .with_node(Some(Arc::clone(node) as Arc<dyn StatusSource>))
        .with_manage(true)
}

fn rpc(mcp: &Mcp, method: &str, params: Value) -> Value {
    mcp.handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
        .unwrap()
}

fn names(mcp: &Mcp) -> Vec<String> {
    rpc(mcp, "tools/list", Value::Null)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn offered_only_when_allowed_and_there_is_a_node() {
    let node = Arc::new(Owned::default());
    let mcp = managed(&node);
    let listed = names(&mcp);
    assert_eq!(&listed[listed.len() - 3..], TOOLS);
    let tools = rpc(&mcp, "tools/list", Value::Null);
    for tool in tools["result"]["tools"].as_array().unwrap() {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
    }
    let init = rpc(
        &mcp,
        "initialize",
        json!({ "protocolVersion": "2025-06-18" }),
    );
    assert!(init["result"]["capabilities"]["prompts"].is_object());
    assert!(init["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("improve_search"));
    let prompts = rpc(&mcp, "prompts/list", Value::Null);
    assert_eq!(prompts["result"]["prompts"][0]["name"], "improve_search");

    let refused = [
        server(Vec::new()).with_manage(true),
        server(Vec::new()).with_node(Some(Arc::clone(&node) as Arc<dyn StatusSource>)),
    ];
    for mcp in refused {
        assert!(!names(&mcp).iter().any(|name| name == "fetch_pages"));
        let init = rpc(
            &mcp,
            "initialize",
            json!({ "protocolVersion": "2025-06-18" }),
        );
        assert!(init["result"]["capabilities"]["prompts"].is_null());
        let prompts = rpc(&mcp, "prompts/list", Value::Null);
        assert_eq!(prompts["result"]["prompts"], json!([]));
        let got = rpc(&mcp, "prompts/get", json!({ "name": "improve_search" }));
        assert_eq!(got["error"]["code"], METHOD_NOT_FOUND);
        for tool in TOOLS {
            let reply = call(&mcp, tool, json!({ "add": ["astronomy"] }));
            assert_eq!(reply["error"]["code"], INVALID_PARAMS, "{tool}");
        }
    }
    assert!(node.settings.lock().unwrap().focus_topics.is_empty());
}

#[test]
fn improve_search_names_the_topic_asked_for() {
    let mcp = managed(&Arc::new(Owned::default()));
    let text = |params: Value| {
        let reply = rpc(&mcp, "prompts/get", params);
        reply["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let plain = text(json!({ "name": "improve_search" }));
    assert!(plain.contains("node_tuning") && plain.contains("fetch_pages"));
    assert!(!plain.contains("{topic}") && !plain.contains("asked for)"));
    let topic = text(json!({ "name": "improve_search", "arguments": { "topic": " Tide pools " } }));
    assert!(topic.contains("starting with \"Tide pools\""), "{topic}");
    let reply = rpc(&mcp, "prompts/get", json!({ "name": "teleport" }));
    assert_eq!(reply["error"]["code"], INVALID_PARAMS);
}

#[test]
fn focus_topics_adds_and_removes_as_the_settings_page_does() {
    let node = Arc::new(Owned::default());
    node.settings.lock().unwrap().focus_topics = vec!["Rust".into()];
    let mcp = managed(&node);
    let reply = call(&mcp, "focus_topics", json!({}));
    assert_eq!(
        reply["result"]["structuredContent"]["focus_topics"],
        json!(["Rust"])
    );
    assert_eq!(*node.changes.lock().unwrap(), 0);

    let reply = call(
        &mcp,
        "focus_topics",
        json!({ "add": ["astronomy", "rust", "  "], "remove": ["RUST"] }),
    );
    assert_eq!(
        reply["result"]["structuredContent"]["focus_topics"],
        json!(["astronomy"])
    );
    assert!(reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Focus topics: astronomy"));
    assert_eq!(node.settings.lock().unwrap().focus_topics, ["astronomy"]);
    assert_eq!(*node.changes.lock().unwrap(), 1);

    let many: Vec<String> = (0..=MAX_FOCUS_TOPICS)
        .map(|n| format!("topic {n}"))
        .collect();
    let reply = call(&mcp, "focus_topics", json!({ "add": many }));
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(*node.changes.lock().unwrap(), 1);
    let reply = call(&mcp, "focus_topics", json!({ "add": "astronomy" }));
    assert_eq!(reply["error"]["code"], INVALID_PARAMS);
}

#[test]
fn fetch_pages_starts_on_the_site_given_and_lists_what_it_keeps() {
    let node = Arc::new(Owned::default());
    let mcp = managed(&node);
    let reply = call(
        &mcp,
        "fetch_pages",
        json!({ "site": "nasa.gov", "about": "Exoplanet discoveries" }),
    );
    let answer = &reply["result"]["structuredContent"];
    assert_eq!(answer["fetched"], 9);
    assert_eq!(answer["pages"][0]["title"], "Exoplanets");
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Fetched 9 pages of nasa.gov"), "{text}");
    assert!(text.contains("- Exoplanets https://science.nasa.gov/exoplanets/"));
    call(
        &mcp,
        "fetch_pages",
        json!({ "site": "https://science.nasa.gov/exoplanets/", "about": "exoplanets" }),
    );
    assert_eq!(
        *node.fetched.lock().unwrap(),
        [
            (
                "https://nasa.gov/".to_string(),
                vec!["discovery".to_string(), "exoplanet".to_string()]
            ),
            (
                "https://science.nasa.gov/exoplanets/".to_string(),
                vec!["exoplanet".to_string()]
            ),
        ]
    );

    for arguments in [
        json!({ "site": "nasa.gov" }),
        json!({ "site": "nasa.gov", "about": "the of" }),
        json!({ "site": "nasa.gov", "about": "one two three four five six seven eight nine" }),
        json!({ "site": "localhost", "about": "exoplanets" }),
        json!({ "site": "ftp://nasa.gov/", "about": "exoplanets" }),
        json!({ "site": "http://127.0.0.1/", "about": "exoplanets" }),
        json!({ "site": "https://nasa.gov:6379/", "about": "exoplanets" }),
    ] {
        let reply = call(&mcp, "fetch_pages", arguments.clone());
        assert_eq!(reply["error"]["code"], INVALID_PARAMS, "{arguments}");
    }
    assert_eq!(node.fetched.lock().unwrap().len(), 2);
}

#[test]
fn node_tuning_shows_what_was_asked() {
    let node = Arc::new(Owned::default());
    node.settings.lock().unwrap().focus_topics = vec!["astronomy".into()];
    let reply = call(&managed(&node), "node_tuning", json!({}));
    let answer = &reply["result"]["structuredContent"];
    assert_eq!(answer["focus_topics"], json!(["astronomy"]));
    assert_eq!(answer["asked_sites"][0]["site"], "nasa.gov");
    assert!(answer.get("fixed_focus_topics").is_none());
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    for line in [
        "Focus topics: astronomy",
        "Crawls wait: Background updates are off",
        "Pages kept for AI apps' asks: 3 on 1 sites.",
        "- nasa.gov: 3 pages, asked 0 days ago, still looking for: discoveries exoplanet",
    ] {
        assert!(text.contains(line), "{line:?} in {text}");
    }
}
