//! Write Plumb Search plugins in Rust.
//!
//! A plugin adds results from a source Plumb does not crawl itself, such
//! as a site's own search API. It is a WebAssembly module that a node's
//! owner installs on their own node; the node runs it in a sandbox where
//! it can do only what this crate offers: read the query, fetch from the
//! hosts its `plugin.json` lists, and hand back results. It cannot read
//! files, open other connections or see who searched.
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
//! - `input_len() -> i32` and `input_read(ptr)`: the [`Query`] as JSON.
//! - `fetch(ptr, len) -> i32`: sends the [`Request`] at `ptr` as JSON;
//!   the body's length, or one of the negative `FETCH_*` codes.
//! - `fetch_status() -> i32` and `body_read(ptr)`: the last response.
//! - `output(ptr, len)`: the [`Output`] as JSON.
//! - `log(ptr, len)`: a line for the node's log, as UTF-8.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The version of the interface; a node runs only plugins of its own.
pub const ABI: i32 = 1;

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
}

/// One result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub title: String,
    /// An `http` or `https` address; others are left out.
    pub url: String,
    /// A line or two about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// When it was published, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<u64>,
}

impl Item {
    pub fn new(title: impl Into<String>, url: impl Into<String>) -> Self {
        Item {
            title: title.into(),
            url: url.into(),
            snippet: None,
            published: None,
        }
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

/// What a plugin hands back.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Output {
    #[serde(default)]
    pub results: Vec<Item>,
    /// Why there are none, for the node's log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// An HTTP request to one of the plugin's hosts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// `GET` or `POST`.
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

impl Request {
    pub fn get(url: impl Into<String>) -> Self {
        Request {
            method: "GET".into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn post(url: impl Into<String>, body: impl Into<String>) -> Self {
        Request {
            method: "POST".into(),
            url: url.into(),
            headers: Vec::new(),
            body: Some(body.into()),
        }
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
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
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
}

/// Runs `search` on the node's query and hands its results back; what
/// [`plugin!`] exports.
#[doc(hidden)]
pub fn run(search: fn(&Query) -> Result<Vec<Item>, Error>) {
    let output = match serde_json::from_slice::<Query>(&host::input()) {
        Ok(query) => match search(&query) {
            Ok(results) => Output {
                results,
                error: None,
            },
            Err(error) => Output {
                results: Vec::new(),
                error: Some(error.to_string()),
            },
        },
        Err(error) => Output {
            results: Vec::new(),
            error: Some(format!("unreadable query: {error}")),
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
            Ok(Response { status, body })
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
        };
        assert_eq!(ok.json::<serde_json::Value>().unwrap()["a"], 1);
        let missing = Response {
            status: 404,
            body: Vec::new(),
        };
        assert_eq!(
            missing.json::<serde_json::Value>().unwrap_err(),
            Error::Status(404)
        );
    }
}
