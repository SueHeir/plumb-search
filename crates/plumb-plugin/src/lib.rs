//! Write Plumb Search plugins in Rust.
//!
//! A plugin adds results from a source Plumb does not crawl itself, such
//! as a site's own search API. It is a WebAssembly module that a node's
//! owner installs on their own node; the node runs it in a sandbox where
//! it can do only what this crate offers: read the query, fetch from the
//! hosts its `plugin.json` lists, and hand back results. It cannot read
//! files, open other connections or see who searched. A plugin may also
//! offer actions on its results ("Add", "Download"), buttons the node's
//! owner can press; see [`Action`] and [`plugin!`]. And it may build a
//! set of items ahead of time, on a schedule, that the node keeps and
//! signs as its owner's; see [`publish!`].
//!
//! ```ignore
//! use plumb_plugin::{get, encode, Item, Query, Error};
//!
//! fn search(query: &Query) -> Result<Vec<Item>, Error> {
//!     let url = format!("https://api.example.org/search?q={}", encode(&query.terms));
//!     let found: Found = get(&url)?.json()?;
//!     Ok(found.items.into_iter().map(|i| Item::new(i.title, i.url)).collect())
//! }
//!
//! plumb_plugin::plugin!(search);
//! ```
//!
//! Build it with `cargo build --release --target wasm32-unknown-unknown`
//! as a `cdylib`, and see `docs/plugins.md` for the rest.
//!
//! # The interface
//!
//! The types here are also what the node reads and writes, as JSON. A
//! plugin exports its `memory`, `plumb_abi` (returning [`ABI`]) and
//! `plumb_search`, which takes and returns nothing; everything else goes
//! through the functions the node gives in the `plumb` import module:
//!
//! - `input_len() -> i32` and `input_read(ptr)`: the [`Query`] as JSON
//!   (for `plumb_act`, the [`ActInput`]).
//! - `fetch(ptr, len) -> i32`: sends the [`Request`] at `ptr` as JSON;
//!   the body's length, or one of the negative `FETCH_*` codes.
//! - `fetch_status() -> i32` and `body_read(ptr)`: the last response.
//! - `headers_len() -> i32` and `headers_read(ptr)`: the last response's
//!   headers, as a JSON list of `[name, value]` pairs.
//! - `output(ptr, len)`: the [`Output`] as JSON.
//! - `log(ptr, len)`: a line for the node's log, as UTF-8.
//! - `emit(ptr, len) -> i32`: from `plumb_publish` only, a JSON list of
//!   [`SetItem`]s for the set it builds; 0, or one of the negative
//!   `EMIT_*` codes.
//!
//! A plugin with actions also exports `plumb_act`, which the node calls
//! when the owner presses one of its buttons, a plugin that marks up the
//! node's own results exports `plumb_annotate` (see [`annotate!`]), and
//! one that builds a set exports `plumb_publish`, which gets a
//! [`PublishInput`] (see [`publish!`]).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The version of the interface. A node runs plugins of its own version
/// and the ones before it; version 2 added response headers, actions,
/// images, badges, [`About`] and page lookups, and version 3 sets built
/// ahead of time ([`publish!`]).
pub const ABI: i32 = 3;

/// `fetch` codes: the host is not one the plugin's `plugin.json` lists.
pub const FETCH_NOT_ALLOWED: i32 = -1;
/// The request failed: no connection, a bad address or a timeout.
pub const FETCH_FAILED: i32 = -2;
/// The plugin made as many requests as one search may.
pub const FETCH_TOO_MANY: i32 = -3;
/// The response is bigger than a plugin may read.
pub const FETCH_TOO_LARGE: i32 = -4;
/// The search's time is up.
pub const FETCH_TIME_UP: i32 = -5;
/// The request is not a [`Request`].
pub const FETCH_BAD_REQUEST: i32 = -6;

/// `emit` codes: items are emitted only by `plumb_publish`.
pub const EMIT_NOT_PUBLISHING: i32 = -1;
/// The run emitted as many items as one run may; it should stop.
pub const EMIT_TOO_MANY: i32 = -2;
/// What was emitted is not a JSON list of items.
pub const EMIT_BAD: i32 = -3;

/// What was searched, as the node hands it to a plugin.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Query {
    /// The whole query, as typed.
    pub text: String,
    /// The query without the keyword that picked this plugin ("rust async"
    /// for "reddit rust async"); the whole query when no keyword did.
    pub terms: String,
    /// The keyword from `plugin.json` that picked this plugin, if one did.
    #[serde(default)]
    pub keyword: Option<String>,
    /// Safe search: `off`, `moderate` or `strict`. The node also leaves
    /// out results that say they are adult, but a source that can filter
    /// on its own should be asked to.
    #[serde(default)]
    pub safe: String,
    /// The language asked for, a code such as `en`, if any.
    #[serde(default)]
    pub language: Option<String>,
    /// The node owner's settings for this plugin, from its `config.json`
    /// (an API key, say); `null` without one.
    #[serde(default)]
    pub config: serde_json::Value,
    /// What the node took the search to be about, when it recognised
    /// one thing: a film, a company, a person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<About>,
    /// For a page lookup (see `pages` in `docs/plugins.md`), the address
    /// of the page the searcher is looking at; `terms` is then empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
}

/// The one thing a search is about, as the node recognised it from
/// Wikipedia and Wikidata, or a song or album from MusicBrainz.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct About {
    /// Its name: the Wikipedia article's title ("Paddington 2").
    pub title: String,
    /// A short description ("2017 film").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Its Wikidata item, such as `Q25188`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wikidata: Option<String>,
    /// Its identifiers on other services, by the service's key in
    /// Plumb's list of profiles: `imdb` (`tt4468740`), `tmdb-movie`,
    /// `tmdb-tv`, `musicbrainz-artist`, `steam`, `github` and so on.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub ids: std::collections::BTreeMap<String, String>,
    /// Who it is by, for a song or album the node knows from MusicBrainz
    /// ("Radiohead" for the song "Creep"); `musicbrainz-recording` or
    /// `musicbrainz-album` in `ids` says which.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

impl About {
    /// Its identifier on service `key`, if the node knows it.
    pub fn id(&self, key: &str) -> Option<&str> {
        self.ids.get(key).map(String::as_str)
    }
}

/// One result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub title: String,
    /// An `http`, `https` or `magnet` address; others are left out.
    pub url: String,
    /// A line or two about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// When it was published, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<u64>,
    /// A picture for it (a poster, a cover), on one of the plugin's hosts.
    /// The node fetches it and puts it in the page, so the searcher's
    /// browser asks nobody for it; PNG, JPEG, GIF or WebP, up to 256 KB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// A word or two shown beside the title: "In library", "45%".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<String>,
    /// Buttons for it, up to three, that the node's owner can press.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
}

/// A button on a result. Pressing it runs the plugin's `act` function
/// (see [`plugin!`]) with `data`, on the node, for the node's owner only.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Action {
    /// What the button says: "Add", "Download".
    pub label: String,
    /// What `act` needs to do it: an id, a link. At most 4 KB as JSON.
    #[serde(default)]
    pub data: serde_json::Value,
}

impl Action {
    pub fn new(label: impl Into<String>, data: serde_json::Value) -> Self {
        Action {
            label: label.into(),
            data,
        }
    }
}

/// What `act` gets when the owner presses a button.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ActInput {
    /// The [`Action::data`] of the button pressed.
    #[serde(default)]
    pub data: serde_json::Value,
    /// The node owner's settings for this plugin, as in [`Query::config`].
    #[serde(default)]
    pub config: serde_json::Value,
}

impl Item {
    pub fn new(title: impl Into<String>, url: impl Into<String>) -> Self {
        Item {
            title: title.into(),
            url: url.into(),
            ..Item::default()
        }
    }

    pub fn image(mut self, url: impl Into<String>) -> Self {
        self.image = Some(url.into());
        self
    }

    pub fn badge(mut self, badge: impl Into<String>) -> Self {
        self.badge = Some(badge.into());
        self
    }

    pub fn action(mut self, action: Action) -> Self {
        self.actions.push(action);
        self
    }

    pub fn snippet(mut self, snippet: impl Into<String>) -> Self {
        self.snippet = Some(snippet.into());
        self
    }

    pub fn published(mut self, unix_seconds: u64) -> Self {
        self.published = Some(unix_seconds);
        self
    }
}

/// The node's own results on a page, as `plumb_annotate` gets them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Shown {
    /// The search, as typed.
    pub query: String,
    /// The results, in the order shown.
    #[serde(default)]
    pub results: Vec<ShownResult>,
    /// The node owner's settings for this plugin, as in [`Query::config`].
    #[serde(default)]
    pub config: serde_json::Value,
}

/// One of the node's own results: a site, or an article about one thing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ShownResult {
    /// Which result it is, for [`Note::new`].
    pub id: u32,
    pub url: String,
    pub title: String,
    /// The site it is on: `wikipedia.org` for an article.
    #[serde(default)]
    pub site: String,
    /// What it is about, when the node knows: a film's article, or the
    /// article a site's result carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<About>,
}

/// What a plugin adds to one of the node's own results, or that it hides
/// it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Note {
    /// The [`ShownResult::id`] it is about.
    pub id: u32,
    /// A word or two shown with it: "Not in library", "Downloaded".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<String>,
    /// Buttons for it, up to three, for the node's owner.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
    /// Leaves the result off the page.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hide: bool,
}

impl Note {
    pub fn new(id: u32) -> Self {
        Note {
            id,
            ..Note::default()
        }
    }

    pub fn badge(mut self, badge: impl Into<String>) -> Self {
        self.badge = Some(badge.into());
        self
    }

    pub fn action(mut self, action: Action) -> Self {
        self.actions.push(action);
        self
    }

    pub fn hide(mut self) -> Self {
        self.hide = true;
        self
    }
}

/// What a plugin hands back.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Output {
    #[serde(default)]
    pub results: Vec<Item>,
    /// What `plumb_annotate` adds to the node's own results.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
    /// What an action did, for the owner who pressed its button.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Why there are none, or why the action failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What `plumb_publish` saves for its next run ([`Published::state`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<serde_json::Value>,
    /// Whether what `plumb_publish` emitted is the whole set
    /// ([`Published::replace`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replace: bool,
}

/// What `plumb_publish` gets: the set it builds, its settings, and what it
/// saved last time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PublishInput {
    /// The set's name, as `publish.set` in `plugin.json` gives it.
    pub set: String,
    /// The node owner's settings for this plugin, as in [`Query::config`].
    #[serde(default)]
    pub config: serde_json::Value,
    /// What the last run that worked saved ([`Published::state`]); `null`
    /// on the first run.
    #[serde(default)]
    pub state: serde_json::Value,
    /// When this run started, in Unix seconds.
    #[serde(default)]
    pub now: u64,
}

/// How a run of `plumb_publish` ends, besides the items it emitted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Published {
    /// Kept for the next run: where it got to, the newest date it saw.
    /// Up to 64 KB as JSON. The node keeps it only when the run worked.
    pub state: serde_json::Value,
    /// What this run emitted is the whole set, and anything it did not
    /// emit is gone. Without it, what a run emits changes the set as it
    /// was: an item replaces the one with its id, and [`SetItem::gone`]
    /// takes one away.
    pub replace: bool,
}

impl Published {
    /// A run that changed the set as it was, and saves `state`.
    pub fn changes(state: serde_json::Value) -> Self {
        Published {
            state,
            replace: false,
        }
    }

    /// A run that emitted the whole set, and saves `state`.
    pub fn whole_set(state: serde_json::Value) -> Self {
        Published {
            state,
            replace: true,
        }
    }
}

/// One item of a set a plugin builds, in Plumb's data format: a thing
/// with a kind, a title and a web address, or an extension that adds to
/// another item (`extends`). The node checks each one as it comes:
/// addresses are put in their canonical form and only `http` and `https`
/// ones are kept, text is trimmed, and fields must be as `plugin.json`
/// declares them; see `docs/plugins.md`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SetItem {
    /// Its id in the set: the source's own stable id ("8863"), never its
    /// title. An item emitted again with the same id replaces it.
    pub id: String,
    /// What it is (`story`, `recipe`), one of `publish.kinds`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// The item it adds to, as a ref: `url:https://...`, `site:...`,
    /// `wd:Q42`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    /// Set on an item that is gone: `{"at": "<date>"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Other names it goes by, searched as names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// One line under the title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// A snippet, up to 1,000 characters, searched as its body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Its language, such as `en`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// How popular it is within the set (points, stars).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
    /// When it was published, as a date: `2026-10-12` or
    /// `2026-10-12T09:30:00Z` ([`date`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    /// When it stops being worth showing, as a date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    /// Its ids elsewhere, by namespace: `wd` (Wikidata), `doi`, `isbn`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub ids: std::collections::BTreeMap<String, String>,
    /// `adult` for an adult item; `personal` keeps it from ever being
    /// shared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Links to other things: `{"rel": "by", "to": "wd:Q42"}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<Edge>,
    /// The fields `publish.fields` declares, by name.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub fields: std::collections::BTreeMap<String, serde_json::Value>,
}

/// A link from a [`SetItem`] to another thing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    /// What the link says: `by`, `about`, `part_of`, `links_to`.
    pub rel: String,
    /// What it links to, as a ref: `wd:Q42`, `url:https://...`.
    pub to: String,
}

impl SetItem {
    /// A thing of `kind` named `title`, at `url`.
    pub fn new(
        id: impl Into<String>,
        kind: impl Into<String>,
        title: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        SetItem {
            id: id.into(),
            kind: kind.into(),
            title: Some(title.into()),
            url: Some(url.into()),
            ..SetItem::default()
        }
    }

    /// An extension that adds to the item `target` names (`wd:Q42`,
    /// `url:https://...`), for a set whose `publish.extends` lists that
    /// kind of ref.
    pub fn extending(id: impl Into<String>, target: impl Into<String>) -> Self {
        SetItem {
            id: id.into(),
            extends: Some(target.into()),
            ..SetItem::default()
        }
    }

    /// Takes the item with this id out of the set, as of `now` (Unix
    /// seconds).
    pub fn gone(id: impl Into<String>, now: u64) -> Self {
        SetItem {
            id: id.into(),
            deleted: Some(serde_json::json!({ "at": date(now) })),
            ..SetItem::default()
        }
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.names.push(name.into());
        self
    }

    pub fn about(mut self, about: impl Into<String>) -> Self {
        self.about = Some(about.into());
        self
    }

    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    pub fn lang(mut self, lang: impl Into<String>) -> Self {
        self.lang = Some(lang.into());
        self
    }

    pub fn weight(mut self, weight: f64) -> Self {
        self.weight = Some(weight);
        self
    }

    /// When it was published, in Unix seconds.
    pub fn published(mut self, unix_seconds: u64) -> Self {
        self.published = Some(date(unix_seconds));
        self
    }

    pub fn id_on(mut self, namespace: impl Into<String>, id: impl Into<String>) -> Self {
        self.ids.insert(namespace.into(), id.into());
        self
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.labels.push(label.into());
        self
    }

    pub fn edge(mut self, rel: impl Into<String>, to: impl Into<String>) -> Self {
        self.edges.push(Edge {
            rel: rel.into(),
            to: to.into(),
        });
        self
    }

    pub fn field(mut self, name: impl Into<String>, value: impl Serialize) -> Self {
        if let Ok(value) = serde_json::to_value(value) {
            self.fields.insert(name.into(), value);
        }
        self
    }
}

/// `unix_seconds` as a UTC date and time, `2026-10-12T09:30:00Z`, the
/// form dates take in sets.
pub fn date(unix_seconds: u64) -> String {
    let days = (unix_seconds / 86_400) as i64;
    let rest = unix_seconds % 86_400;
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// The most `emit` sends to the node at once, as JSON.
const EMIT_BATCH: usize = 512 * 1024;

/// Adds `items` to the set `plumb_publish` is building. Call it as often
/// as you like: the node writes what it gets as it comes, so a set can be
/// much bigger than the plugin's memory. An item the node refuses is left
/// out and logged; an error here means the run should stop.
pub fn emit(items: &[SetItem]) -> Result<(), Error> {
    let mut batch = Vec::with_capacity(EMIT_BATCH.min(items.len() * 256 + 2));
    batch.push(b'[');
    for item in items {
        let json = serde_json::to_vec(item)?;
        if batch.len() > 1 && batch.len() + json.len() + 2 > EMIT_BATCH {
            batch.push(b']');
            send_batch(&batch)?;
            batch.truncate(1);
        }
        if batch.len() > 1 {
            batch.push(b',');
        }
        batch.extend_from_slice(&json);
    }
    if batch.len() > 1 {
        batch.push(b']');
        send_batch(&batch)?;
    }
    Ok(())
}

fn send_batch(json: &[u8]) -> Result<(), Error> {
    match host::emit(json) {
        0 => Ok(()),
        EMIT_NOT_PUBLISHING => Err(Error::Other(
            "items are emitted only by plumb_publish".into(),
        )),
        EMIT_TOO_MANY => Err(Error::Other(
            "the run emitted as many items as one run may".into(),
        )),
        _ => Err(Error::Other("the node could not read the items".into())),
    }
}

/// An HTTP request to one of the plugin's hosts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// `GET`, `POST`, `PUT`, `PATCH`, `DELETE` or `HEAD`.
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

impl Request {
    /// A request with `method` and no body.
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Request {
            method: method.into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn get(url: impl Into<String>) -> Self {
        Request::new("GET", url)
    }

    pub fn post(url: impl Into<String>, body: impl Into<String>) -> Self {
        Request::new("POST", url).body(body)
    }

    pub fn put(url: impl Into<String>, body: impl Into<String>) -> Self {
        Request::new("PUT", url).body(body)
    }

    pub fn delete(url: impl Into<String>) -> Self {
        Request::new("DELETE", url)
    }

    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Sends it. An answer with any status is `Ok`; see
    /// [`Response::ok`].
    pub fn send(&self) -> Result<Response, Error> {
        let request = serde_json::to_vec(self).map_err(|e| Error::Other(e.to_string()))?;
        host::fetch(&request)
    }
}

/// An answer to a [`Request`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Response {
    pub status: u16,
    /// Its headers, names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// The first header named `name` (in any case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The cookies the answer sets, as a `Cookie` header's value for the
    /// next request: `SID=abc; lang=en`. Empty without any.
    pub fn cookies(&self) -> String {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("set-cookie"))
            .filter_map(|(_, v)| v.split(';').next())
            .map(str::trim)
            .filter(|pair| pair.contains('='))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// The response, or [`Error::Status`] unless its status is 2xx.
    pub fn ok(self) -> Result<Self, Error> {
        if (200..300).contains(&self.status) {
            Ok(self)
        } else {
            Err(Error::Status(self.status))
        }
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body as JSON, when the status is 2xx.
    pub fn json<T: DeserializeOwned>(self) -> Result<T, Error> {
        let ok = self.ok()?;
        serde_json::from_slice(&ok.body).map_err(|e| Error::Json(e.to_string()))
    }
}

/// Why a plugin found nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The host is not in the plugin's `plugin.json`.
    NotAllowed,
    /// The request failed: no connection, a bad address or a timeout.
    Failed,
    /// One search may make only a few requests.
    TooManyRequests,
    /// The response was too big.
    TooLarge,
    /// The search's time ran out.
    TimeUp,
    /// The answer's status was not 2xx.
    Status(u16),
    /// The answer was not the JSON expected.
    Json(String),
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotAllowed => write!(f, "that host is not in plugin.json"),
            Error::Failed => write!(f, "the request failed"),
            Error::TooManyRequests => write!(f, "too many requests for one search"),
            Error::TooLarge => write!(f, "the response is too big"),
            Error::TimeUp => write!(f, "the search's time is up"),
            Error::Status(status) => write!(f, "the answer's status was {status}"),
            Error::Json(error) => write!(f, "unexpected JSON: {error}"),
            Error::Other(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::Json(error.to_string())
    }
}

/// The [`Error`] for a negative `fetch` code.
pub fn fetch_error(code: i32) -> Error {
    match code {
        FETCH_NOT_ALLOWED => Error::NotAllowed,
        FETCH_TOO_MANY => Error::TooManyRequests,
        FETCH_TOO_LARGE => Error::TooLarge,
        FETCH_TIME_UP => Error::TimeUp,
        FETCH_BAD_REQUEST => Error::Other("the request is not valid".into()),
        _ => Error::Failed,
    }
}

/// Fetches `url` with GET.
pub fn get(url: &str) -> Result<Response, Error> {
    Request::get(url).send()
}

/// A line in the node's log, to debug a plugin with `plumb try-plugin`.
pub fn log(line: &str) {
    host::log(line);
}

/// `text` percent-encoded for a URL's query string.
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Makes `search` the plugin's search: `plugin!(search)` once in the
/// crate, where `search` is a `fn(&Query) -> Result<Vec<Item>, Error>`.
///
/// A plugin whose results have [`Action`]s names its `act` too:
/// `plugin!(search, act)`, where `act` is a
/// `fn(&ActInput) -> Result<String, Error>` that does what the button
/// says and returns a line for the owner ("Added Paddington 2").
#[macro_export]
macro_rules! plugin {
    ($search:path) => {
        #[no_mangle]
        pub extern "C" fn plumb_abi() -> i32 {
            $crate::ABI
        }

        #[no_mangle]
        pub extern "C" fn plumb_search() {
            $crate::run($search)
        }
    };
    ($search:path, $act:path) => {
        $crate::plugin!($search);

        #[no_mangle]
        pub extern "C" fn plumb_act() {
            $crate::run_act($act)
        }
    };
}

/// Runs `search` on the node's query and hands its results back; what
/// [`plugin!`] exports.
#[doc(hidden)]
pub fn run(search: fn(&Query) -> Result<Vec<Item>, Error>) {
    let output = match serde_json::from_slice::<Query>(&host::input()) {
        Ok(query) => match search(&query) {
            Ok(results) => Output {
                results,
                ..Output::default()
            },
            Err(error) => Output {
                error: Some(error.to_string()),
                ..Output::default()
            },
        },
        Err(error) => Output {
            error: Some(format!("unreadable query: {error}")),
            ..Output::default()
        },
    };
    let json = serde_json::to_vec(&output).unwrap_or_else(|_| b"{}".to_vec());
    host::output(&json);
}

/// Makes `annotate` the plugin's way of marking up the node's own
/// results: `annotate!(annotate)` once in the crate, where `annotate` is a
/// `fn(&Shown) -> Result<Vec<Note>, Error>`. It can go with [`plugin!`] or
/// alone, for a plugin that only marks up results.
#[macro_export]
macro_rules! annotate {
    ($annotate:path) => {
        #[no_mangle]
        pub extern "C" fn plumb_annotate() {
            $crate::run_annotate($annotate)
        }
    };
}

/// [`annotate!`] without [`plugin!`] still needs the interface version.
#[macro_export]
macro_rules! annotate_only {
    ($annotate:path) => {
        #[no_mangle]
        pub extern "C" fn plumb_abi() -> i32 {
            $crate::ABI
        }

        $crate::annotate!($annotate);
    };
}

/// Runs `annotate` on the node's results and hands back its notes; what
/// [`annotate!`] exports as `plumb_annotate`.
#[doc(hidden)]
pub fn run_annotate(annotate: fn(&Shown) -> Result<Vec<Note>, Error>) {
    let output = match serde_json::from_slice::<Shown>(&host::input()) {
        Ok(shown) => match annotate(&shown) {
            Ok(notes) => Output {
                notes,
                ..Output::default()
            },
            Err(error) => Output {
                error: Some(error.to_string()),
                ..Output::default()
            },
        },
        Err(error) => Output {
            error: Some(format!("unreadable results: {error}")),
            ..Output::default()
        },
    };
    let json = serde_json::to_vec(&output).unwrap_or_else(|_| b"{}".to_vec());
    host::output(&json);
}

/// Makes `publish` the plugin's way of building a set ahead of time:
/// `publish!(publish)` once in the crate, where `publish` is a
/// `fn(&PublishInput) -> Result<Published, Error>` that [`emit`]s the
/// set's items and says what to save for the next run. It can go with
/// [`plugin!`] and [`annotate!`], or alone with [`publish_only!`]. The
/// node runs it on the schedule `publish.every` in `plugin.json` sets.
#[macro_export]
macro_rules! publish {
    ($publish:path) => {
        #[no_mangle]
        pub extern "C" fn plumb_publish() {
            $crate::run_publish($publish)
        }
    };
}

/// [`publish!`] without [`plugin!`] still needs the interface version.
#[macro_export]
macro_rules! publish_only {
    ($publish:path) => {
        #[no_mangle]
        pub extern "C" fn plumb_abi() -> i32 {
            $crate::ABI
        }

        $crate::publish!($publish);
    };
}

/// Runs `publish` and hands back how it ended; what [`publish!`] exports
/// as `plumb_publish`.
#[doc(hidden)]
pub fn run_publish(publish: fn(&PublishInput) -> Result<Published, Error>) {
    let output = match serde_json::from_slice::<PublishInput>(&host::input()) {
        Ok(input) => match publish(&input) {
            Ok(published) => Output {
                state: Some(published.state),
                replace: published.replace,
                ..Output::default()
            },
            Err(error) => Output {
                error: Some(error.to_string()),
                ..Output::default()
            },
        },
        Err(error) => Output {
            error: Some(format!("unreadable input: {error}")),
            ..Output::default()
        },
    };
    let json = serde_json::to_vec(&output).unwrap_or_else(|_| b"{}".to_vec());
    host::output(&json);
}

/// Runs `act` on the button the owner pressed and hands back what it
/// did; what [`plugin!`] exports as `plumb_act`.
#[doc(hidden)]
pub fn run_act(act: fn(&ActInput) -> Result<String, Error>) {
    let output = match serde_json::from_slice::<ActInput>(&host::input()) {
        Ok(input) => match act(&input) {
            Ok(message) => Output {
                message: Some(message),
                ..Output::default()
            },
            Err(error) => Output {
                error: Some(error.to_string()),
                ..Output::default()
            },
        },
        Err(error) => Output {
            error: Some(format!("unreadable action: {error}")),
            ..Output::default()
        },
    };
    let json = serde_json::to_vec(&output).unwrap_or_else(|_| b"{}".to_vec());
    host::output(&json);
}

#[cfg(target_arch = "wasm32")]
mod host {
    use super::{fetch_error, Error, Response};

    mod ffi {
        #[link(wasm_import_module = "plumb")]
        extern "C" {
            pub fn input_len() -> i32;
            pub fn input_read(ptr: *mut u8);
            pub fn fetch(ptr: *const u8, len: i32) -> i32;
            pub fn fetch_status() -> i32;
            pub fn body_read(ptr: *mut u8);
            pub fn headers_len() -> i32;
            pub fn headers_read(ptr: *mut u8);
            pub fn output(ptr: *const u8, len: i32);
            pub fn log(ptr: *const u8, len: i32);
            pub fn emit(ptr: *const u8, len: i32) -> i32;
        }
    }

    pub fn input() -> Vec<u8> {
        // SAFETY: the node writes exactly `input_len` bytes at the pointer.
        unsafe {
            let len = usize::try_from(ffi::input_len()).unwrap_or(0);
            let mut buffer = vec![0u8; len];
            ffi::input_read(buffer.as_mut_ptr());
            buffer
        }
    }

    pub fn fetch(request: &[u8]) -> Result<Response, Error> {
        // SAFETY: the node reads `len` bytes at the pointer, and writes
        // exactly the returned length into a buffer of that size.
        unsafe {
            let len = ffi::fetch(request.as_ptr(), request.len() as i32);
            if len < 0 {
                return Err(fetch_error(len));
            }
            let mut body = vec![0u8; len as usize];
            ffi::body_read(body.as_mut_ptr());
            let status = u16::try_from(ffi::fetch_status()).unwrap_or(0);
            let mut headers = vec![0u8; usize::try_from(ffi::headers_len()).unwrap_or(0)];
            ffi::headers_read(headers.as_mut_ptr());
            let headers = serde_json::from_slice(&headers).unwrap_or_default();
            Ok(Response {
                status,
                headers,
                body,
            })
        }
    }

    pub fn output(json: &[u8]) {
        // SAFETY: the node reads `len` bytes at the pointer.
        unsafe { ffi::output(json.as_ptr(), json.len() as i32) }
    }

    pub fn log(line: &str) {
        // SAFETY: the node reads `len` bytes at the pointer.
        unsafe { ffi::log(line.as_ptr(), line.len() as i32) }
    }

    pub fn emit(json: &[u8]) -> i32 {
        // SAFETY: the node reads `len` bytes at the pointer.
        unsafe { ffi::emit(json.as_ptr(), json.len() as i32) }
    }
}

/// Outside WebAssembly there is no node to ask: a plugin's code builds
/// and its own tests run, but it fetches nothing.
#[cfg(not(target_arch = "wasm32"))]
mod host {
    use super::{Error, Response};

    pub fn input() -> Vec<u8> {
        Vec::new()
    }

    pub fn fetch(_request: &[u8]) -> Result<Response, Error> {
        Err(Error::Other(
            "plugins fetch only inside a Plumb node".into(),
        ))
    }

    pub fn output(_json: &[u8]) {}

    pub fn log(line: &str) {
        eprintln!("{line}");
    }

    pub fn emit(_json: &[u8]) -> i32 {
        super::EMIT_NOT_PUBLISHING
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_query_words_for_a_url() {
        assert_eq!(encode("rust async"), "rust%20async");
        assert_eq!(encode("c++ & you?"), "c%2B%2B%20%26%20you%3F");
        assert_eq!(encode("café"), "caf%C3%A9");
    }

    #[test]
    fn items_leave_out_what_they_lack() {
        let json = serde_json::to_string(&Item::new("A", "https://a.example/")).unwrap();
        assert_eq!(json, r#"{"title":"A","url":"https://a.example/"}"#);
        let query: Query = serde_json::from_str(r#"{"text":"hn rust","terms":"rust"}"#).unwrap();
        assert_eq!(query.terms, "rust");
        assert!(query.config.is_null());
    }

    #[test]
    fn only_2xx_answers_read_as_json() {
        let ok = Response {
            status: 200,
            body: br#"{"a":1}"#.to_vec(),
            ..Response::default()
        };
        assert_eq!(ok.json::<serde_json::Value>().unwrap()["a"], 1);
        let missing = Response {
            status: 404,
            ..Response::default()
        };
        assert_eq!(
            missing.json::<serde_json::Value>().unwrap_err(),
            Error::Status(404)
        );
    }

    #[test]
    fn cookies_carry_over_without_their_attributes() {
        let login = Response {
            status: 200,
            headers: vec![
                ("set-cookie".into(), "SID=abc; HttpOnly; path=/".into()),
                ("content-type".into(), "text/plain".into()),
                ("Set-Cookie".into(), "lang=en".into()),
            ],
            body: Vec::new(),
        };
        assert_eq!(login.cookies(), "SID=abc; lang=en");
        assert_eq!(login.header("Content-Type"), Some("text/plain"));
    }

    #[test]
    fn notes_leave_out_what_they_lack() {
        let note = Note::new(3).badge("Not in library");
        assert_eq!(
            serde_json::to_string(&note).unwrap(),
            r#"{"id":3,"badge":"Not in library"}"#
        );
        let hidden: Note = serde_json::from_str(r#"{"id":1,"hide":true}"#).unwrap();
        assert_eq!(hidden, Note::new(1).hide());
    }

    #[test]
    fn set_items_leave_out_what_they_lack() {
        let item = SetItem::new("8863", "story", "My YC app", "https://dropbox.com/")
            .field("points", 104)
            .published(1_175_714_200);
        assert_eq!(
            serde_json::to_string(&item).unwrap(),
            r#"{"id":"8863","kind":"story","title":"My YC app","url":"https://dropbox.com/","published":"2007-04-04T19:16:40Z","fields":{"points":104}}"#
        );
        assert_eq!(
            serde_json::to_string(&SetItem::gone("8863", 0)).unwrap(),
            r#"{"id":"8863","deleted":{"at":"1970-01-01T00:00:00Z"}}"#
        );
    }

    #[test]
    fn dates_in_utc() {
        assert_eq!(date(0), "1970-01-01T00:00:00Z");
        assert_eq!(date(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(date(1_791_806_399), "2026-10-12T11:59:59Z");
        assert_eq!(date(4_102_444_800), "2100-01-01T00:00:00Z");
    }

    #[test]
    fn emitting_outside_a_node_fails() {
        assert!(emit(&[]).is_ok());
        assert!(emit(&[SetItem::gone("1", 0)]).is_err());
    }

    #[test]
    fn actions_and_about_read_back() {
        let item = Item::new("A", "magnet:?xt=urn:btih:x")
            .badge("New")
            .action(Action::new("Add", serde_json::json!({"id": 7})));
        let json = serde_json::to_string(&item).unwrap();
        assert_eq!(serde_json::from_str::<Item>(&json).unwrap(), item);
        let query: Query = serde_json::from_str(
            r#"{"text":"paddington 2","terms":"paddington 2",
                "about":{"title":"Paddington 2","ids":{"imdb":"tt4468740"}}}"#,
        )
        .unwrap();
        assert_eq!(query.about.unwrap().id("imdb"), Some("tt4468740"));
    }
}
