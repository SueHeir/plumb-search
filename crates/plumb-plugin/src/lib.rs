//! Write Plumb Search plugins in Rust.
//!
//! A plugin adds results from a source Plumb does not crawl itself, such
//! as a site's own search API. It is a WebAssembly module that a node's
//! owner installs on their own node; the node runs it in a sandbox where
//! it can do only what this crate offers: read the query, fetch from the
//! hosts its `plugin.json` lists, and hand back results. It cannot read
//! files, open other connections or see who searched. A plugin may also
//! offer actions on its results ("Add", "Download"), buttons the node's
//! owner can press; see [`Action`] and [`plugin!`].
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
//!
//! A plugin with actions also exports `plumb_act`, which the node calls
//! when the owner presses one of its buttons, and a plugin that marks up
//! the node's own results exports `plumb_annotate` (see [`annotate!`]).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The version of the interface. A node runs plugins of its own version
/// and the ones before it; version 2 added response headers, actions,
/// images, badges, [`About`] and page lookups.
pub const ABI: i32 = 2;

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
