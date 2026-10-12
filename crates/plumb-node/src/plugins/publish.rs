//! Sets that plugins build ahead of time (plugin interface 3, `plugins.md`
//! in the design notes).
//!
//! A plugin that exports `plumb_publish` and has a `publish` section in its
//! `plugin.json` runs on a schedule rather than during a search. It
//! [`emit`](plumb_plugin::emit)s items in the data format, a batch at a
//! time. The node checks each one as it comes, as it checks any set:
//! canonical `http` and `https` addresses, text in NFC and within the caps,
//! one of the kinds the plugin declared, and fields of the types it
//! declared. An item the node's denylist takes off is left out.
//!
//! What a run emits changes the set as it was, unless the run says it
//! emitted the whole set. The node then writes a new generation of the set
//! and signs it with its owner's publisher key ([`KEY_FILE`]): the owner
//! publishes what their plugins build, not the plugin's author. A small
//! `state` the plugin hands back is kept for its next run, which only a
//! run that worked replaces.
//!
//! Local first: the sets are kept in [`OWN_DIR`], which the node never
//! serves to other nodes, under the scope [`OWN_SCOPE`]. A plugin marked
//! `personal` labels its set and every item in it `personal`, so it can
//! never be shared. Sharing a set, which needs its owner's choice and a
//! license that allows it, and indexing it, are not built yet.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use ed25519_dalek::SigningKey;
use plumb_core::storage::{self, StorageBudget};
use plumb_format::deny::Denylist;
use plumb_format::item::MAX_TEXT_CHARS;
use plumb_format::manifest::is_set_name;
use plumb_format::urls::canonical_url;
use plumb_format::value::normalize_text;
use plumb_format::{FieldSpec, Item, Manifest as SetManifest, PublicKey};
use plumb_plugin::{PublishInput, EMIT_BAD, EMIT_TOO_MANY};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::{Limits, Plugin};
use crate::sets;

/// The folder in a node's data folder that holds the sets its owner
/// publishes. The node serves nothing in it to other nodes.
pub const OWN_DIR: &str = "own-sets";
/// The scope of the sets a node's plugins build: `local/hn-stories`.
pub const OWN_SCOPE: &str = "local";
/// The owner's publisher key, in the data folder, made on the first run.
pub const KEY_FILE: &str = "keys/publisher.key";

/// How long one run may take.
const PUBLISH_TIME: Duration = Duration::from_secs(30 * 60);
/// About this many WebAssembly instructions for one run.
const PUBLISH_FUEL: u64 = 200_000_000_000;
/// Requests one run may make.
const PUBLISH_FETCHES: usize = 1000;
/// The least time between two requests of one run to one host.
const PACE: Duration = Duration::from_secs(1);
/// The most a plugin's saved state may hold, as JSON.
const MAX_STATE: usize = 64 * 1024;
/// The most items one run may emit, and one set may hold.
pub const MAX_ITEMS: u64 = 250_000;
/// How often a plugin runs, unless its `publish.every` says.
const EVERY: Duration = Duration::from_secs(86_400);
const MIN_EVERY: Duration = Duration::from_secs(3600);
const MAX_EVERY: Duration = Duration::from_secs(30 * 86_400);
/// Kinds only the project's own sets have.
const RESERVED_KINDS: &[&str] = &["deny", "site-rule"];

/// A plugin's `publish` section: the set it builds.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Publish {
    /// The set's name: lower-case letters, digits and dashes
    /// (`hn-stories`). The node keeps it as `local/<set>`.
    pub set: String,
    /// How often it runs: `6h`, `1d`, `7d`; from an hour to 30 days, a day
    /// without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<String>,
    /// What the set is called; the plugin's name without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The kinds its items may have (`story`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    /// The kinds of refs its extensions name (`url`, `site`, `wd`), for a
    /// set that adds to other items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extends: Vec<String>,
    /// The license of what it emits, an SPDX expression; `unknown`, the
    /// default, keeps the set on this node for good.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Where its items come from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Its items' fields, by name, with their types and roles.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, FieldSpec>,
    /// How its items are shown: templates with `{field}` slots.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub show: BTreeMap<String, String>,
    /// Where its items go among results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<Value>,
    /// Personal data (the owner's own library or bookmarks): the set and
    /// every item in it are labelled `personal` and never shared.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub personal: bool,
}

impl Publish {
    /// The set's full name, `local/<set>`.
    pub fn set_name(&self) -> String {
        format!("{OWN_SCOPE}/{}", self.set)
    }

    /// How often it runs.
    pub fn every(&self) -> Result<Duration> {
        let Some(every) = &self.every else {
            return Ok(EVERY);
        };
        let every = every.trim();
        let (number, unit) = every.split_at(every.len().saturating_sub(1));
        let unit = match unit {
            "h" => 3600,
            "d" => 86_400,
            _ => bail!("every {every:?} is not a number of hours or days, such as 6h or 1d"),
        };
        let number: u64 = number
            .parse()
            .with_context(|| format!("every {every:?} is not a number of hours or days"))?;
        let every = Duration::from_secs(number.saturating_mul(unit));
        ensure!(
            (MIN_EVERY..=MAX_EVERY).contains(&every),
            "every must be from 1h to 30d"
        );
        Ok(every)
    }

    /// The license it names, unless it names none or `unknown`.
    pub fn license(&self) -> Option<&str> {
        self.license
            .as_deref()
            .map(str::trim)
            .filter(|license| !license.is_empty() && !license.eq_ignore_ascii_case("unknown"))
    }

    /// Checks the section: a set name, a schedule, kinds or refs to
    /// extend, and declarations a set's manifest may have.
    pub(super) fn check(&self) -> Result<()> {
        ensure!(
            is_set_name(&self.set_name()),
            "set {:?} must be 1 to 64 lower-case letters, digits and dashes",
            self.set
        );
        self.every()?;
        ensure!(
            !self.kinds.is_empty() || !self.extends.is_empty(),
            "it needs the kinds of its items, or the refs its items extend"
        );
        for kind in &self.kinds {
            ensure!(
                !kind.trim().is_empty() && !RESERVED_KINDS.contains(&kind.as_str()),
                "{kind:?} is not a kind it may publish"
            );
        }
        let mut manifest = self.manifest("plugin", None);
        manifest.insert(
            "files".into(),
            json!([{"path": "items-001.jsonl.zst", "count": 1, "bytes": 1, "sha256": "0".repeat(64)}]),
        );
        template(manifest)?.check()
    }

    /// The set's manifest, before its parts, kinds and fields are known
    /// and it is signed: what `plugin_name` (and `about`) publishes.
    fn manifest(&self, plugin_name: &str, about: Option<&str>) -> Map<String, Value> {
        let mut manifest = Map::new();
        manifest.insert("format".into(), json!(plumb_format::FORMAT));
        manifest.insert("set".into(), json!(self.set_name()));
        manifest.insert("generation".into(), json!(sets::now()));
        manifest.insert(
            "title".into(),
            json!(self.title.as_deref().unwrap_or(plugin_name)),
        );
        if let Some(about) = about.filter(|about| !about.trim().is_empty()) {
            manifest.insert("about".into(), json!(about));
        }
        if let Some(source) = &self.source {
            manifest.insert("source".into(), json!(source));
        }
        if let Some(license) = self.license() {
            manifest.insert("license".into(), json!(license));
        }
        manifest.insert("every".into(), json!(self.every.as_deref().unwrap_or("1d")));
        if !self.extends.is_empty() {
            manifest.insert("extends".into(), json!(self.extends));
        }
        if self.personal {
            manifest.insert("labels".into(), json!(["personal"]));
        }
        if !self.show.is_empty() {
            manifest.insert("show".into(), json!(self.show));
        }
        if let Some(place) = &self.place {
            manifest.insert("place".into(), place.clone());
        }
        manifest
    }
}

/// `manifest` with every field a set's manifest needs to be read, so that
/// items can be checked against it before the set is written.
fn template(mut manifest: Map<String, Value>) -> Result<SetManifest> {
    manifest
        .entry("publisher")
        .or_insert(json!("ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
    manifest.entry("files").or_insert(json!([]));
    serde_json::from_value(Value::Object(manifest)).context("making the set's manifest")
}

/// Takes the items a run of `plumb_publish` emits, checks them, and writes
/// them to a file as they come.
pub(super) struct Emitter {
    manifest: SetManifest,
    kinds: Vec<String>,
    personal: bool,
    denylist: Option<Arc<Denylist>>,
    out: BufWriter<File>,
    /// Each id emitted, with the line of the last item emitted with it.
    ids: HashMap<String, u64>,
    lines: u64,
    refused: u64,
    first_refused: Option<String>,
    denied: u64,
    failed: Option<String>,
}

impl Emitter {
    fn create(
        path: &Path,
        publish: &Publish,
        manifest: SetManifest,
        denylist: Option<Arc<Denylist>>,
    ) -> Result<Self> {
        let file = File::create(path).with_context(|| format!("making {}", path.display()))?;
        Ok(Emitter {
            manifest,
            kinds: publish.kinds.clone(),
            personal: publish.personal,
            denylist,
            out: BufWriter::new(file),
            ids: HashMap::new(),
            lines: 0,
            refused: 0,
            first_refused: None,
            denied: 0,
            failed: None,
        })
    }

    /// Takes a batch, a JSON list of items: 0, or an `EMIT_*` code.
    pub(super) fn take(&mut self, json: &[u8]) -> i32 {
        let Ok(values) = serde_json::from_slice::<Vec<Value>>(json) else {
            return EMIT_BAD;
        };
        for value in values {
            if self.lines >= MAX_ITEMS || self.failed.is_some() {
                return EMIT_TOO_MANY;
            }
            match self.tidy(value) {
                Ok(Some(item)) => {
                    let written = serde_json::to_writer(&mut self.out, &item)
                        .map_err(anyhow::Error::from)
                        .and_then(|()| Ok(self.out.write_all(b"\n")?));
                    if let Err(err) = written {
                        self.failed = Some(format!("writing what it emitted: {err}"));
                        return EMIT_TOO_MANY;
                    }
                    self.ids.insert(item.id, self.lines);
                    self.lines += 1;
                }
                Ok(None) => self.denied += 1,
                Err(err) => {
                    self.refused += 1;
                    self.first_refused.get_or_insert(format!("{err:#}"));
                }
            }
        }
        0
    }

    /// `value` as an item of the set, tidied and checked; `None` when the
    /// denylist takes it off.
    fn tidy(&self, value: Value) -> Result<Option<Item>> {
        let mut item: Item = serde_json::from_value(value).context("not an item")?;
        if let Some(url) = item.url.take() {
            item.url = Some(
                canonical_url(&url)
                    .with_context(|| format!("{url:?} is not an http or https address"))?,
            );
            // The node works out the site of an item with an address.
            item.site = None;
        }
        for text in [&mut item.title, &mut item.about, &mut item.text] {
            *text = text
                .take()
                .map(|text| normalize_text(text.trim()))
                .filter(|text| !text.is_empty());
        }
        if let Some(text) = &mut item.text {
            if text.chars().count() > MAX_TEXT_CHARS {
                *text = text.chars().take(MAX_TEXT_CHARS).collect();
            }
        }
        let mut names = Vec::new();
        for name in std::mem::take(&mut item.names) {
            let name = normalize_text(name.trim());
            if !name.is_empty() && !names.contains(&name) && item.title.as_ref() != Some(&name) {
                names.push(name);
            }
        }
        item.names = names;
        if !item.is_extension() && !item.is_deleted() {
            ensure!(
                self.kinds.contains(&item.kind),
                "item {:?}: kind {:?} is not one of publish.kinds",
                item.id,
                item.kind
            );
        }
        if self.personal && item.may_share() {
            item.labels.push("personal".into());
        }
        self.manifest.check_item(&item)?;
        if !item.is_deleted() {
            if let Some(list) = &self.denylist {
                if list.denies(&item, None).is_some() {
                    return Ok(None);
                }
            }
        }
        Ok(Some(item))
    }
}

/// What one run of a plugin's `plumb_publish` emitted.
struct Emitted {
    /// The file of the items it emitted, one per line.
    path: PathBuf,
    ids: HashMap<String, u64>,
    lines: u64,
    refused: u64,
    first_refused: Option<String>,
    denied: u64,
    state: Value,
    replace: bool,
}

impl Plugin {
    /// Whether it builds a set ahead of time.
    pub fn publishes(&self) -> bool {
        self.publishes
    }

    /// Runs its `plumb_publish` once, blocking, with what it saved last
    /// `state`, writing what it emits to `path`.
    fn run_publish(
        &self,
        state: Value,
        now: u64,
        path: &Path,
        denylist: Option<Arc<Denylist>>,
        stop: Option<tokio::sync::watch::Receiver<bool>>,
        runtime: &tokio::runtime::Handle,
    ) -> Result<Emitted> {
        let publish = self
            .manifest
            .publish
            .as_ref()
            .context("the plugin publishes nothing")?;
        let manifest = template(self.set_manifest(publish))?;
        let emitter = Emitter::create(path, publish, manifest, denylist)?;
        let input = PublishInput {
            set: publish.set.clone(),
            config: self.config.clone(),
            state,
            now,
        };
        let limits = Limits {
            deadline: Instant::now() + PUBLISH_TIME,
            fuel: PUBLISH_FUEL,
            fetches: PUBLISH_FETCHES,
            pace: Some(PACE),
            stop,
        };
        let (output, emitter) = self.call_with(
            "plumb_publish",
            serde_json::to_vec(&input)?,
            limits,
            Some(emitter),
            runtime,
        )?;
        let mut emitter = emitter.context("the run lost what it emitted")?;
        if let Some(failed) = emitter.failed {
            bail!("{failed}");
        }
        emitter.out.flush()?;
        emitter.out.get_ref().sync_all()?;
        let state = output.state.unwrap_or(Value::Null);
        ensure!(
            serde_json::to_vec(&state)?.len() <= MAX_STATE,
            "the state it saved is bigger than {} KB",
            MAX_STATE / 1024
        );
        Ok(Emitted {
            path: path.to_path_buf(),
            ids: emitter.ids,
            lines: emitter.lines,
            refused: emitter.refused,
            first_refused: emitter.first_refused,
            denied: emitter.denied,
            state,
            replace: output.replace,
        })
    }

    /// Its set's manifest, before the parts are written and it is signed.
    fn set_manifest(&self, publish: &Publish) -> Map<String, Value> {
        let mut manifest = publish.manifest(&self.manifest.name, Some(&self.manifest.about));
        if !publish.fields.is_empty() {
            manifest.insert("fields".into(), json!(publish.fields));
        }
        manifest
    }
}

/// What a run did, as kept in its [`RunNote`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ran {
    /// The generation it wrote; `None` when the set came out empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    /// Items in the set after it.
    pub items: u64,
    /// Items it emitted, refused ones and denied ones aside.
    pub emitted: u64,
    /// Items the node refused, and why the first one was.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub refused: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_refused: Option<String>,
    /// Items the denylist took off.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub denied: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Kept beside a plugin's set as `<set>.run.json`: when it last ran, what
/// it saved, and how the last run went.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunNote {
    /// The plugin's folder name.
    #[serde(default)]
    pub plugin: String,
    /// What the last run that worked saved, for the next one.
    #[serde(default)]
    pub state: Value,
    /// When it last ran, worked or not, in Unix seconds.
    #[serde(default)]
    pub ran_at: u64,
    /// Why the last run failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What the last run that worked did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<Ran>,
}

/// The folder of the set `set` (`local/hn-stories`) the owner publishes.
pub fn own_set_dir(data: &Path, set: &str) -> PathBuf {
    data.join(OWN_DIR).join(set)
}

fn note_path(data: &Path, set: &str) -> PathBuf {
    beside(&own_set_dir(data, set), "run.json")
}

/// The note on the last run of `plugin`, or an empty one.
pub fn read_note(data: &Path, plugin: &Plugin) -> RunNote {
    let Some(publish) = &plugin.manifest.publish else {
        return RunNote::default();
    };
    fs::read(note_path(data, &publish.set_name()))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .filter(|note: &RunNote| note.plugin == plugin.id)
        .unwrap_or_default()
}

fn write_note(path: &Path, note: &RunNote) -> Result<()> {
    let partial = beside(path, "partial");
    fs::write(&partial, serde_json::to_vec_pretty(note)?)
        .with_context(|| format!("writing {}", partial.display()))?;
    fs::rename(&partial, path).with_context(|| format!("writing {}", path.display()))
}

/// The owner's publisher key in `data`, made the first time.
pub fn own_key(data: &Path) -> Result<SigningKey> {
    let path = data.join(KEY_FILE);
    if path.exists() {
        return sets::read_key(&path);
    }
    let dir = path.parent().expect("the key is in a folder");
    fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let key = sets::new_key(&path)?;
    tracing::info!(
        "made this node's publisher key {}, which signs the sets its plugins build",
        PublicKey::of(&key)
    );
    Ok(key)
}

/// What a run of a plugin's `plumb_publish` needs from the node.
pub struct Owner<'a> {
    /// The data folder.
    pub data: &'a Path,
    /// The owner's publisher key ([`own_key`]).
    pub key: &'a SigningKey,
    pub denylist: Option<Arc<Denylist>>,
    pub budget: Option<&'a StorageBudget>,
    pub stop: Option<tokio::sync::watch::Receiver<bool>>,
}

/// Runs `plugin`'s `plumb_publish` once, blocking, and writes the new
/// generation of its set, signed with the owner's key. The run's note is
/// written whether it worked or not; a run that failed leaves the set
/// and the saved state as they were.
pub fn publish_set(
    plugin: &Plugin,
    owner: &Owner<'_>,
    now: u64,
    runtime: &tokio::runtime::Handle,
) -> Result<Ran> {
    let publish = plugin
        .manifest
        .publish
        .as_ref()
        .context("the plugin publishes nothing")?;
    let set = publish.set_name();
    let dir = own_set_dir(owner.data, &set);
    let scope = dir.parent().expect("a set is in its scope's folder");
    fs::create_dir_all(scope).with_context(|| format!("making {}", scope.display()))?;
    let mut note = read_note(owner.data, plugin);
    let (emitted_path, staging) = (beside(&dir, "emitted"), beside(&dir, "new"));
    let _ = fs::remove_file(&emitted_path);
    let _ = fs::remove_dir_all(&staging);
    let ran = plugin
        .run_publish(
            note.state.clone(),
            now,
            &emitted_path,
            owner.denylist.clone(),
            owner.stop.clone(),
            runtime,
        )
        .and_then(|emitted| {
            let ran = write_generation(plugin, publish, owner, &dir, &staging, &emitted)?;
            Ok((emitted.state, ran))
        });
    let _ = fs::remove_file(&emitted_path);
    let _ = fs::remove_dir_all(&staging);
    note.plugin = plugin.id.clone();
    note.ran_at = now;
    let result = match ran {
        Ok((state, ran)) => {
            note.state = state;
            note.error = None;
            note.last = Some(ran.clone());
            Ok(ran)
        }
        Err(err) => {
            note.error = Some(format!("{err:#}"));
            Err(err)
        }
    };
    write_note(&note_path(owner.data, &set), &note)?;
    result
}

/// Writes the set in `dir` again in `staging` with what a run `emitted`,
/// signs it and puts it in place.
fn write_generation(
    plugin: &Plugin,
    publish: &Publish,
    owner: &Owner<'_>,
    dir: &Path,
    staging: &Path,
    emitted: &Emitted,
) -> Result<Ran> {
    let previous: Option<SetManifest> = if emitted.replace {
        None
    } else {
        match fs::read_to_string(dir.join(sets::MANIFEST)) {
            Ok(text) => Some(serde_json::from_str(&text).context("reading the set as it was")?),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err).context("reading the set as it was"),
        }
    };
    let failed = std::cell::RefCell::new(None);
    let fail = |err: anyhow::Error| {
        failed.borrow_mut().get_or_insert(err);
    };
    let kept = previous
        .iter()
        .flat_map(|manifest| sets::items_in(dir, manifest))
        .filter_map(|item| item.map_err(fail).ok())
        .filter(|item| !emitted.ids.contains_key(&item.id));
    let reader = BufReader::new(
        File::open(&emitted.path).with_context(|| format!("reading {}", emitted.path.display()))?,
    );
    let new = reader.lines().zip(0u64..).filter_map(|(line, n)| {
        let item: Item = line
            .map_err(anyhow::Error::from)
            .and_then(|line| Ok(serde_json::from_str(&line)?))
            .map_err(fail)
            .ok()?;
        (emitted.ids.get(&item.id) == Some(&n) && !item.is_deleted()).then_some(item)
    });
    let mut items = kept.chain(new).take(MAX_ITEMS as usize).peekable();
    let ran = |generation: Option<String>, items: u64| Ran {
        generation,
        items,
        emitted: emitted.lines,
        refused: emitted.refused,
        first_refused: emitted.first_refused.clone(),
        denied: emitted.denied,
    };
    if items.peek().is_none() {
        if let Some(err) = failed.into_inner() {
            return Err(err);
        }
        // Nothing is left: the set is gone too.
        storage::remove_directory(dir, owner.budget)?;
        return Ok(ran(None, 0));
    }
    let mut manifest = plugin.set_manifest(publish);
    manifest.remove("fields");
    let generation = manifest
        .get("generation")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let written = sets::write_set(
        items.map(Some),
        publish.fields.clone(),
        manifest,
        staging,
        sets::PART_ENDS,
        sets::LEVEL,
    )?;
    if let Some(err) = failed.into_inner() {
        return Err(err);
    }
    let prev = previous
        .as_ref()
        .map(|manifest| manifest.generation.clone())
        .filter(|prev| *prev < generation);
    let checked = sets::sign_set(staging, owner.key, prev.as_deref(), None)?;
    let bytes = storage::directory_bytes(staging)?;
    if let Some(budget) = owner.budget {
        ensure!(
            budget.available() >= bytes,
            "no room under the storage limit for {} ({bytes} bytes)",
            publish.set_name()
        );
        budget.added(bytes);
    }
    swap(dir, staging, owner.budget)?;
    Ok(ran(Some(checked.generation), written.items))
}

/// Puts the set in `staging` in place of the one in `dir`.
fn swap(dir: &Path, staging: &Path, budget: Option<&StorageBudget>) -> Result<()> {
    let old = beside(dir, "old");
    storage::remove_directory(&old, budget)?;
    if dir.exists() {
        fs::rename(dir, &old).with_context(|| format!("moving {}", dir.display()))?;
    }
    fs::rename(staging, dir).with_context(|| format!("moving {}", staging.display()))?;
    storage::remove_directory(&old, budget)?;
    Ok(())
}

/// `path` with `.<suffix>` added to its name.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

/// What `plumb try-plugin --publish` prints: a run of the plugin's
/// `plumb_publish`, the items it emitted as the node took them, and what
/// it would save. Nothing is kept.
pub fn try_publish(
    plugin: &Plugin,
    state: Value,
    runtime: &tokio::runtime::Handle,
) -> Result<Value> {
    let dir = tempfile::tempdir().context("making a folder for what it emits")?;
    let path = dir.path().join("emitted.jsonl");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let emitted = plugin.run_publish(state, now, &path, None, None, runtime)?;
    let mut items = Vec::new();
    for (line, n) in BufReader::new(File::open(&path)?).lines().zip(0u64..) {
        let item: Item = serde_json::from_str(&line?)?;
        if emitted.ids.get(&item.id) == Some(&n) {
            items.push(item);
        }
    }
    let set = plugin
        .manifest
        .publish
        .as_ref()
        .map(Publish::set_name)
        .unwrap_or_default();
    Ok(json!({
        "set": set,
        "replace": emitted.replace,
        "items": items,
        "refused": emitted.refused,
        "first_refused": emitted.first_refused,
        "state": emitted.state,
    }))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) fn publishing_plugin(publish: Publish, wat: &str) -> Plugin {
    let manifest = super::Manifest {
        name: "Test".into(),
        hosts: vec!["a.example".into()],
        publish: Some(publish),
        ..super::Manifest::default()
    };
    Plugin::from_parts("test".into(), manifest, Value::Null, wat.as_bytes()).expect("a plugin")
}
