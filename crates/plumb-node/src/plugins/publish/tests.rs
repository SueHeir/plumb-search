use super::*;
use plumb_format::deny::Switch;

/// A plugin module whose `plumb_publish` emits each of `batches` (JSON
/// lists of items) and hands back `output`.
fn publishing(batches: &[&str], output: &str) -> String {
    let mut data = String::new();
    let mut calls = String::new();
    let mut at = 0;
    for batch in batches.iter().chain([&output]) {
        let escaped: String = batch.bytes().map(|b| format!("\\{b:02x}")).collect();
        data += &format!("(data (i32.const {at}) \"{escaped}\")\n");
        calls += &if std::ptr::eq(batch, &output) {
            format!(
                "(call $output (i32.const {at}) (i32.const {}))\n",
                batch.len()
            )
        } else {
            format!(
                "(drop (call $emit (i32.const {at}) (i32.const {})))\n",
                batch.len()
            )
        };
        at += batch.len();
    }
    format!(
        r#"(module
            (import "plumb" "emit" (func $emit (param i32 i32) (result i32)))
            (import "plumb" "output" (func $output (param i32 i32)))
            (memory (export "memory") 1)
            {data}
            (func (export "plumb_abi") (result i32) (i32.const 3))
            (func (export "plumb_publish") {calls}))"#
    )
}

fn stories() -> Publish {
    serde_json::from_value(json!({
        "set": "stories",
        "every": "6h",
        "kinds": ["story"],
        "source": "https://news.example/api",
        "fields": {"points": {"type": "number", "roles": ["show"]}},
    }))
    .unwrap()
}

struct Node {
    data: tempfile::TempDir,
    key: SigningKey,
    runtime: tokio::runtime::Runtime,
}

impl Node {
    fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let key = own_key(data.path()).unwrap();
        Node {
            data,
            key,
            runtime: tokio::runtime::Runtime::new().unwrap(),
        }
    }

    fn run(&self, plugin: &Plugin, denylist: Option<Arc<Denylist>>) -> Result<Ran> {
        let owner = Owner {
            data: self.data.path(),
            key: &self.key,
            denylist,
            budget: None,
            stop: None,
        };
        publish_set(plugin, &owner, 1_791_806_400, self.runtime.handle())
    }

    /// The set as a node would take it: checked against the owner's key.
    fn set(&self, set: &str) -> (SetManifest, Vec<Item>) {
        let dir = own_set_dir(self.data.path(), set);
        sets::check_set(&dir, Some(&PublicKey::of(&self.key)), &[]).unwrap();
        sets::read_items(&dir).unwrap()
    }
}

fn titles(items: &[Item]) -> Vec<(&str, &str)> {
    items
        .iter()
        .map(|item| (item.id.as_str(), item.title.as_deref().unwrap_or_default()))
        .collect()
}

#[test]
fn a_run_keeps_what_it_emits_as_a_set_the_owner_signs() {
    let node = Node::new();
    let first = publishing_plugin(
        stories(),
        &publishing(
            &[
                r#"[{"id":"1","kind":"story","title":" One ","url":"HTTPS://News.Example/a?utm_source=x","fields":{"points":5}},
                    {"id":"2","kind":"story","title":"Two","url":"https://news.example/b"}]"#,
                r#"[{"id":"3","kind":"recipe","title":"Not a story","url":"https://news.example/c"},
                    {"id":"4","kind":"story","title":"No web address","url":"ftp://news.example/d"},
                    {"id":"5","kind":"story","title":"Undeclared","url":"https://news.example/e","fields":{"stars":1}},
                    "not an item"]"#,
            ],
            r#"{"state":{"after":"2"}}"#,
        ),
    );
    let ran = node.run(&first, None).unwrap();
    assert_eq!((ran.items, ran.emitted, ran.refused), (2, 2, 4));
    assert!(ran
        .first_refused
        .as_deref()
        .unwrap()
        .contains("not one of publish.kinds"));
    let (manifest, items) = node.set("local/stories");
    assert_eq!(manifest.publisher, PublicKey::of(&node.key));
    assert_eq!(manifest.title.as_deref(), Some("Test"));
    assert_eq!(manifest.every.as_deref(), Some("6h"));
    assert_eq!(manifest.license, None, "unknown keeps it local");
    assert_eq!(manifest.kinds, ["story"]);
    assert!(manifest.fields.contains_key("points"));
    assert_eq!(titles(&items), [("1", "One"), ("2", "Two")]);
    assert_eq!(
        items[0].url.as_deref(),
        Some("https://news.example/a?utm_source=x")
    );
    let note = read_note(node.data.path(), &first);
    assert_eq!(note.state, json!({"after": "2"}));
    assert_eq!(note.ran_at, 1_791_806_400);
    assert_eq!(note.last, Some(ran));

    // The next run changes the set as it was.
    let next = publishing_plugin(
        stories(),
        &publishing(
            &[r#"[{"id":"1","deleted":{"at":"2026-10-12"}},
                    {"id":"2","kind":"story","title":"Two, edited","url":"https://news.example/b"},
                    {"id":"6","kind":"story","title":"Six","url":"https://news.example/f"},
                    {"id":"6","kind":"story","title":"Six, edited","url":"https://news.example/f"}]"#],
            r#"{"state":{"after":"6"}}"#,
        ),
    );
    let ran = node.run(&next, None).unwrap();
    assert_eq!((ran.items, ran.emitted, ran.refused), (2, 4, 0));
    let (_, items) = node.set("local/stories");
    assert_eq!(titles(&items), [("2", "Two, edited"), ("6", "Six, edited")]);
    assert_eq!(
        read_note(node.data.path(), &next).state,
        json!({"after": "6"})
    );

    // A run that fails leaves the set and the state as they were.
    let failing = publishing_plugin(
        stories(),
        &publishing(
            &[r#"[{"id":"7","kind":"story","title":"Seven","url":"https://news.example/g"}]"#],
            r#"{"error":"the source is down"}"#,
        ),
    );
    let err = node.run(&failing, None).unwrap_err();
    assert!(err.to_string().contains("the source is down"), "{err:#}");
    let (_, items) = node.set("local/stories");
    assert_eq!(titles(&items), [("2", "Two, edited"), ("6", "Six, edited")]);
    let note = read_note(node.data.path(), &failing);
    assert_eq!(note.state, json!({"after": "6"}));
    assert!(note.error.unwrap().contains("the source is down"));

    // A run that emits the whole set replaces it.
    let whole = publishing_plugin(
        stories(),
        &publishing(
            &[r#"[{"id":"9","kind":"story","title":"Nine","url":"https://news.example/i"}]"#],
            r#"{"state":null,"replace":true}"#,
        ),
    );
    node.run(&whole, None).unwrap();
    let (_, items) = node.set("local/stories");
    assert_eq!(titles(&items), [("9", "Nine")]);

    // And one that takes everything away leaves no set.
    let empty = publishing_plugin(
        stories(),
        &publishing(&[r#"[{"id":"9","deleted":{"at":"2026-10-12"}}]"#], r#"{}"#),
    );
    assert_eq!(node.run(&empty, None).unwrap().items, 0);
    assert!(!own_set_dir(node.data.path(), "local/stories").exists());
    // Nothing of it is where the node serves sets from.
    assert!(!node.data.path().join("sets").exists());
}

#[test]
fn a_personal_set_and_its_items_are_labelled_and_the_denylist_applies() {
    let node = Node::new();
    let mut publish = stories();
    publish.set = "library".into();
    publish.personal = true;
    let plugin = publishing_plugin(
        publish,
        &publishing(
            &[
                r#"[{"id":"1","kind":"story","title":"Mine","url":"https://news.example/a"},
                  {"id":"2","kind":"story","title":"Denied","url":"https://www.usbank.com/x"}]"#,
            ],
            r#"{"state":1}"#,
        ),
    );
    let entry: Item = serde_json::from_value(json!({
        "id": "d1", "kind": "deny", "target": "site:usbank.com", "category": "copyright",
        "where": ["*"], "added": "2026-10-12",
    }))
    .unwrap();
    let mut list = Denylist::new(Switch::On);
    list.add_items(&[entry]);
    let ran = node.run(&plugin, Some(Arc::new(list))).unwrap();
    assert_eq!((ran.items, ran.denied), (1, 1));
    let (manifest, items) = node.set("local/library");
    assert!(!manifest.may_share());
    assert!(!items[0].may_share());
}

#[test]
fn plugin_json_and_the_module_must_agree_on_publishing() {
    let wat = publishing(&[], "{}");
    let plugin = |publish: Option<Publish>, wat: &str| {
        let manifest = super::super::Manifest {
            name: "Test".into(),
            hosts: vec!["a.example".into()],
            keywords: vec!["x".into()],
            publish,
            ..super::super::Manifest::default()
        };
        Plugin::from_parts("test".into(), manifest, Value::Null, wat.as_bytes())
    };
    assert!(plugin(Some(stories()), &wat).unwrap().publishes());
    let err = plugin(None, &wat).unwrap_err();
    assert!(format!("{err:#}").contains("no publish section"), "{err:#}");
    let search_only = super::super::answering("{}");
    let err = plugin(Some(stories()), &search_only).unwrap_err();
    assert!(format!("{err:#}").contains("no plumb_publish"), "{err:#}");
    for (key, value, why) in [
        ("every", json!("10m"), "every"),
        ("every", json!("90d"), "every"),
        ("set", json!("Stories"), "lower-case"),
        ("kinds", json!(["deny"]), "not a kind"),
        ("kinds", json!([]), "kinds"),
    ] {
        let mut publish = serde_json::to_value(stories()).unwrap();
        publish[key] = value;
        let publish: Publish = serde_json::from_value(publish).unwrap();
        let err = plugin(Some(publish), &wat).unwrap_err();
        assert!(format!("{err:#}").contains(why), "{key}: {err:#}");
    }
}

#[test]
fn only_publish_may_emit() {
    let emits = r#"(module
        (import "plumb" "emit" (func $emit (param i32 i32) (result i32)))
        (import "plumb" "output" (func $output (param i32 i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "[]{\"results\":[]}{\"error\":\"emitted\"}")
        (func (export "plumb_abi") (result i32) (i32.const 3))
        (func (export "plumb_search")
            (if (i32.eq (call $emit (i32.const 0) (i32.const 2)) (i32.const -1))
                (then (call $output (i32.const 2) (i32.const 14)))
                (else (call $output (i32.const 16) (i32.const 19))))))"#;
    let manifest = super::super::Manifest {
        name: "Test".into(),
        hosts: vec!["a.example".into()],
        keywords: vec!["x".into()],
        ..super::super::Manifest::default()
    };
    let plugin =
        Plugin::from_parts("test".into(), manifest, Value::Null, emits.as_bytes()).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let query = plumb_plugin::Query::default();
    assert_eq!(plugin.run(&query, runtime.handle()).unwrap(), Vec::new());
}

#[test]
fn trying_a_publish_prints_what_it_would_keep() {
    let plugin = publishing_plugin(
        stories(),
        &publishing(
            &[
                r#"[{"id":"1","kind":"story","title":"One","url":"https://news.example/a"},
                  {"id":"1","kind":"story","title":"One, again","url":"https://news.example/a"}]"#,
            ],
            r#"{"state":{"after":"1"}}"#,
        ),
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let tried = try_publish(&plugin, Value::Null, runtime.handle()).unwrap();
    assert_eq!(tried["set"], "local/stories");
    assert_eq!(tried["items"].as_array().unwrap().len(), 1);
    assert_eq!(tried["items"][0]["title"], "One, again");
    assert_eq!(tried["state"], json!({"after": "1"}));
}
