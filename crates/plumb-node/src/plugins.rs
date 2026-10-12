//! Plugins: results from sources Plumb does not crawl, added by a node's
//! owner on their own node.
//!
//! A plugin is a folder in the data folder's `plugins/` with a
//! `plugin.json` (its name, the hosts it may fetch from and what runs
//! it), a `plugin.wasm` built with the `plumb-plugin` crate, and
//! optionally a `config.json` of the owner's settings for it. Nodes come
//! with none, and plumbsearch.org runs none.
//!
//! Each plugin runs in a WebAssembly sandbox (wasmi) with a fuel and
//! memory limit. It can read the query and fetch from its own hosts, a
//! few requests per search, and nothing else: no files, no other
//! connections, nothing about who searched. Its results show only on
//! this node's own pages and APIs, and are never shared with other
//! nodes or kept in records.
//!
//! A plugin may also mark up the node's own results (a badge, buttons,
//! or leaving one out) through its `plumb_annotate`, and put buttons on
//! its results. Pressing one runs the
//! plugin's `plumb_act` in the same sandbox; only the node's owner, on
//! the computer the node runs on, can press them (see `web::plugins`).

use std::collections::{BTreeMap, HashMap};
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use plumb_core::SafeSearch;
use plumb_plugin::{About, ActInput, Item, Note, Output, Query, Request, Shown, ShownResult};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};
use wasmi::{Caller, Config, Engine, Extern, Linker, Memory, Module, Store, StoreLimits};

/// The folder in a node's data folder that holds its plugins.
pub const PLUGINS_DIR: &str = "plugins";
/// The file in [`PLUGINS_DIR`] that keeps the owner's choices of what a
/// search that fits a plugin does ([`Suggest`]), by its folder name.
pub const SUGGEST_FILE: &str = "suggest.json";

/// How long a search waits for a plugin, unless its `plugin.json` says.
const SEARCH_TIME: Duration = Duration::from_secs(4);
/// The most a plugin's `plugin.json` may ask a search to wait for it.
const MAX_SECONDS: u64 = 10;
/// How long an action may take.
const ACT_TIME: Duration = Duration::from_secs(10);
/// Requests one plugin may make for one search or action.
const MAX_FETCHES: usize = 4;
/// The biggest response a plugin may read.
const MAX_BODY: usize = 2 * 1024 * 1024;
/// The biggest request, output or log line a plugin may hand over.
const MAX_HANDED: usize = 1024 * 1024;
/// Response headers a plugin may read, at most.
const MAX_HEADERS: usize = 100;
/// A plugin's memory.
const MAX_MEMORY: usize = 64 * 1024 * 1024;
/// About this many WebAssembly instructions per search: plenty to read a
/// few API answers, not enough to keep a thread busy for long.
const FUEL: u64 = 2_000_000_000;
/// Results kept from one plugin for one search.
const MAX_RESULTS: usize = 10;
/// Buttons kept on one result.
const MAX_ACTIONS: usize = 3;
/// The most an action's data may hold, as JSON.
const MAX_ACTION_DATA: usize = 4096;
/// The biggest picture the node fetches for a result.
const MAX_IMAGE: usize = 256 * 1024;
/// The node's results one plugin is shown, at most.
const MAX_SHOWN: usize = 30;
/// Searches one plugin runs at once; more are left without its results.
const MAX_RUNNING: usize = 4;
/// How long a plugin's results for a query are reused, unless its
/// `plugin.json` says; this keeps a busy node from asking its sources the
/// same thing over and over.
const CACHE_TIME: Duration = Duration::from_secs(600);
/// The longest a `plugin.json` may ask results to be reused.
const MAX_CACHE_SECONDS: u64 = 86_400;
const CACHE_ENTRIES: usize = 1000;
/// How long the results of a plugin run for what a search is about by
/// one of its `run_ids` (a song, an album) are reused, unless its own
/// `cache_seconds` is longer or 0.
const THING_CACHE_TIME: Duration = Duration::from_secs(86_400);

/// A plugin's `plugin.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Manifest {
    /// What the results page calls it: "From Hacker News".
    pub name: String,
    /// A line about where its results come from.
    #[serde(default)]
    pub about: String,
    /// The hosts it may fetch from: `api.example.org`, `*.example.org`
    /// for every subdomain, or with a port, `127.0.0.1:7878`. Without a
    /// port only 80 and 443 are allowed.
    pub hosts: Vec<String>,
    /// Words or phrases that run it when a query starts or ends with
    /// one ("hn", "hacker news"); they are taken off what it searches.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Runs it for every search. Mind the source's limits.
    #[serde(default)]
    pub always: bool,
    /// Runs it, without a keyword, for searches the node recognises as
    /// one thing with an identifier on one of these services (`imdb`,
    /// `tmdb-movie`, `musicbrainz-artist`), or `wikidata` for anything
    /// with a Wikidata item.
    #[serde(default)]
    pub ids: Vec<String>,
    /// Words or phrases that, anywhere in a search, say it fits the
    /// plugin ("lyrics", "music video"), as its `ids` do.
    #[serde(default)]
    pub hints: Vec<String>,
    /// Of its `ids`, those that make a search surely for it, so that it
    /// runs even where a search that fits it is only offered (`suggest`
    /// "button"): a song's or an album's, say, for a video site whose
    /// quota keeps an artist's search behind a link.
    #[serde(default)]
    pub run_ids: Vec<String>,
    /// What a search that fits it does without a keyword, unless the
    /// node's owner chose otherwise: run it, or offer its results.
    #[serde(default)]
    pub suggest: Suggest,
    /// Sites whose pages it can say something about (`*.example.org`),
    /// for `/api/plugins/page`, which a browser extension asks about the
    /// page open in a tab.
    #[serde(default)]
    pub pages: Vec<String>,
    /// How long its results for a search are reused: 0 for never (live
    /// status), at most a day. Ten minutes without it.
    #[serde(default)]
    pub cache_seconds: Option<u64>,
    /// How long a search waits for it, 1 to 10 seconds; 4 without it.
    /// The results page waits for its slowest plugin.
    #[serde(default)]
    pub seconds: Option<u64>,
}

/// What a search that fits a plugin (by its `ids` or `hints`) does when
/// none of its keywords ran it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Suggest {
    /// Runs it: its results show with the node's.
    #[default]
    Automatic,
    /// Shows a link that runs it: for a source with a small daily quota.
    Button,
    /// Nothing: only its keywords run it.
    Keywords,
}

impl Suggest {
    pub const ALL: [Suggest; 3] = [Suggest::Automatic, Suggest::Button, Suggest::Keywords];

    pub fn as_str(self) -> &'static str {
        match self {
            Suggest::Automatic => "automatic",
            Suggest::Button => "button",
            Suggest::Keywords => "keywords",
        }
    }

    pub fn parse(text: &str) -> Option<Suggest> {
        Suggest::ALL.into_iter().find(|s| s.as_str() == text)
    }
}

/// What a search does with one plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pick {
    /// Runs it, with the keyword that did, if one did, and the terms.
    Run(Option<String>, String),
    /// Offers its results behind a link.
    Offer,
}

/// A link on a results page that runs a plugin for a search that fits
/// it: the same search with `run` set to the plugin's folder name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Offer {
    /// The plugin's folder name.
    pub plugin: String,
    /// Its name, from `plugin.json`.
    pub name: String,
}

/// One entry of `hosts` or `pages`: a host name or address, or `*.` and
/// a parent name for its subdomains, and the port it names, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostSpec {
    name: String,
    subdomains: bool,
    port: Option<u16>,
}

impl HostSpec {
    /// `spec` read as `name`, `*.name`, `name:port`, `[v6]` or
    /// `[v6]:port`; `None` when it is none of those.
    fn parse(spec: &str) -> Option<HostSpec> {
        let spec = spec.trim().to_ascii_lowercase();
        let (subdomains, rest) = match spec.strip_prefix("*.") {
            Some(rest) => (true, rest),
            None => (false, spec.as_str()),
        };
        let (name, port) = if let Some(v6) = rest.strip_prefix('[') {
            let (address, after) = v6.split_once(']')?;
            address.parse::<Ipv6Addr>().ok()?;
            if subdomains {
                return None;
            }
            let port = match after {
                "" => None,
                after => Some(after.strip_prefix(':')?),
            };
            (address.to_string(), port)
        } else {
            match rest.split_once(':') {
                Some((name, port)) => (name.to_string(), Some(port)),
                None => (rest.to_string(), None),
            }
        };
        let port = match port {
            Some(port) => Some(port.parse::<u16>().ok().filter(|p| *p > 0)?),
            None => None,
        };
        let v6 = name.contains(':');
        let plain = !name.is_empty()
            && name.len() <= 253
            && !name.starts_with(['.', '-'])
            && !name.ends_with(['.', '-'])
            && !name.contains("..")
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
        (v6 || plain).then_some(HostSpec {
            name,
            subdomains,
            port,
        })
    }

    /// Whether `host` (as a URL gives it, `[::1]` for IPv6) at `port` is
    /// one this entry names. Without a port, only the web's own ports.
    fn allows(&self, host: &str, port: Option<u16>) -> bool {
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let named = if self.subdomains {
            host.strip_suffix(self.name.as_str())
                .is_some_and(|sub| sub.ends_with('.') && sub.len() > 1)
        } else {
            host == self.name
        };
        let port_ok = match (self.port, port) {
            (Some(wanted), Some(port)) => wanted == port,
            (Some(_), None) => false,
            (None, port) => matches!(port, Some(80 | 443) | None),
        };
        named && port_ok
    }
}

impl Manifest {
    /// Whether it is a `plugin.json` the node can run, for a plugin that
    /// `annotates` the node's results or not.
    fn check(&self, annotates: bool) -> Result<()> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 60 {
            bail!("its name must be 1 to 60 characters");
        }
        if self.keywords.iter().all(|k| k.trim().is_empty())
            && !self.always
            && self.ids.iter().all(|k| k.trim().is_empty())
            && self.hints.iter().all(|k| k.trim().is_empty())
            && self.pages.is_empty()
            && !annotates
        {
            bail!("it needs keywords, ids, hints, pages, or \"always\": true");
        }
        for host in &self.hosts {
            if HostSpec::parse(host).is_none() {
                bail!(
                    "{host:?} is not a host such as api.example.org, *.example.org or \
                     127.0.0.1:7878"
                );
            }
        }
        for page in &self.pages {
            if HostSpec::parse(page).is_none_or(|spec| spec.port.is_some()) {
                bail!("{page:?} in pages is not a site such as example.org or *.example.org");
            }
        }
        if self.cache_seconds.is_some_and(|s| s > MAX_CACHE_SECONDS) {
            bail!("cache_seconds may be at most {MAX_CACHE_SECONDS}");
        }
        if self
            .seconds
            .is_some_and(|s| !(1..=MAX_SECONDS).contains(&s))
        {
            bail!("seconds must be 1 to {MAX_SECONDS}");
        }
        Ok(())
    }

    /// Whether the plugin may fetch from `url`.
    fn allows(&self, url: &url::Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        let port = url.port_or_known_default();
        self.hosts
            .iter()
            .filter_map(|h| HostSpec::parse(h))
            .any(|spec| spec.allows(host, port))
    }

    /// Whether it can say something about the page at `url`.
    fn covers_page(&self, url: &url::Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        self.pages
            .iter()
            .filter_map(|p| HostSpec::parse(p))
            .any(|spec| spec.allows(host, None))
    }

    /// What `query` does with it, under the owner's `suggest`: runs it
    /// with the keyword it starts or ends with and the rest of it; for a
    /// plugin that runs on every search, or a search that fits it, runs
    /// it on the whole query or offers it.
    fn picks(&self, query: &str, about: Option<&About>, suggest: Suggest) -> Option<Pick> {
        let words: Vec<&str> = query.split_whitespace().collect();
        let lower: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
        for keyword in &self.keywords {
            let wanted: Vec<String> = keyword.split_whitespace().map(str::to_lowercase).collect();
            let n = wanted.len();
            if n == 0 || n >= words.len() {
                continue;
            }
            if lower[..n] == wanted[..] {
                return Some(Pick::Run(Some(keyword.clone()), words[n..].join(" ")));
            }
            if lower[words.len() - n..] == wanted[..] {
                return Some(Pick::Run(
                    Some(keyword.clone()),
                    words[..words.len() - n].join(" "),
                ));
            }
        }
        if self.always {
            return Some(Pick::Run(None, words.join(" ")));
        }
        let fits = about.is_some_and(|about| self.knows(about)) || self.hinted(&lower);
        let sure = about.is_some_and(|about| self.surely_for(about));
        match suggest {
            _ if !fits => None,
            Suggest::Automatic => Some(Pick::Run(None, words.join(" "))),
            Suggest::Button if sure => Some(Pick::Run(None, words.join(" "))),
            Suggest::Button => Some(Pick::Offer),
            Suggest::Keywords => None,
        }
    }

    /// Whether the search of `words` (lowercase) has one of its `hints`.
    fn hinted(&self, words: &[String]) -> bool {
        self.hints.iter().any(|hint| {
            let wanted: Vec<String> = hint.split_whitespace().map(str::to_lowercase).collect();
            !wanted.is_empty() && words.windows(wanted.len()).any(|w| w == &wanted[..])
        })
    }

    /// Whether a search can fit it without a keyword.
    pub fn can_fit(&self) -> bool {
        !self.always
            && (self.ids.iter().any(|k| !k.trim().is_empty())
                || self.hints.iter().any(|h| !h.trim().is_empty()))
    }

    /// Whether `about` has an identifier its `ids` lists.
    fn knows(&self, about: &About) -> bool {
        self.ids.iter().any(|key| {
            about.ids.contains_key(key) || (key == "wikidata" && about.wikidata.is_some())
        })
    }

    /// Whether `about` has an identifier its `run_ids` lists, of those
    /// its `ids` lists.
    fn surely_for(&self, about: &About) -> bool {
        self.run_ids
            .iter()
            .any(|key| self.ids.contains(key) && about.ids.contains_key(key))
    }

    fn time(&self) -> Duration {
        self.seconds.map_or(SEARCH_TIME, Duration::from_secs)
    }

    fn cache_time(&self) -> Duration {
        self.cache_seconds.map_or(CACHE_TIME, Duration::from_secs)
    }
}

/// One installed plugin.
pub struct Plugin {
    /// Its folder's name.
    pub id: String,
    pub manifest: Manifest,
    config: serde_json::Value,
    engine: Engine,
    module: Module,
    client: reqwest::Client,
    running: Arc<Semaphore>,
    /// Fuel for one search.
    fuel: u64,
    /// Whether it exports `plumb_act`, so its buttons can be pressed.
    acts: bool,
    /// Whether it exports `plumb_annotate`, to mark up the node's results.
    annotates: bool,
}

impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("id", &self.id)
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

/// The functions a plugin may import, all from the `plumb` module.
const IMPORTS: &[&str] = &[
    "input_len",
    "input_read",
    "fetch",
    "fetch_status",
    "body_read",
    "headers_len",
    "headers_read",
    "output",
    "log",
];

impl Plugin {
    /// Loads the plugin in folder `dir`.
    pub fn load(dir: &Path) -> Result<Self> {
        let id = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("plugin")
            .to_string();
        let manifest_path = dir.join("plugin.json");
        let manifest: Manifest = serde_json::from_slice(
            &std::fs::read(&manifest_path)
                .with_context(|| format!("reading {}", manifest_path.display()))?,
        )
        .with_context(|| format!("reading {}", manifest_path.display()))?;
        let config_path = dir.join("config.json");
        let config = match std::fs::read(&config_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", config_path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Value::Null,
            Err(e) => return Err(e).with_context(|| format!("reading {}", config_path.display())),
        };
        let wasm_path = dir.join("plugin.wasm");
        let wasm = std::fs::read(&wasm_path)
            .with_context(|| format!("reading {}", wasm_path.display()))?;
        Self::from_parts(id, manifest, config, &wasm)
            .with_context(|| format!("checking {}", dir.display()))
    }

    /// A plugin from its manifest, settings and module (binary or text).
    pub fn from_parts(
        id: String,
        manifest: Manifest,
        config: serde_json::Value,
        wasm: &[u8],
    ) -> Result<Self> {
        let mut engine_config = Config::default();
        engine_config.consume_fuel(true);
        let engine = Engine::new(&engine_config);
        let module = Module::new(&engine, wasm)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("reading plugin.wasm")?;
        for import in module.imports() {
            if import.module() != "plumb" || !IMPORTS.contains(&import.name()) {
                bail!(
                    "plugin.wasm asks for {}.{}, which Plumb does not give plugins",
                    import.module(),
                    import.name()
                );
            }
        }
        let acts = module.exports().any(|export| export.name() == "plumb_act");
        let annotates = module
            .exports()
            .any(|export| export.name() == "plumb_annotate");
        manifest.check(annotates)?;
        let allowed = manifest.clone();
        let client = reqwest::Client::builder()
            .user_agent(format!(
                "PlumbSearch/{} plugin {id} (+https://github.com/SueHeir/plumb-search)",
                env!("CARGO_PKG_VERSION")
            ))
            .connect_timeout(Duration::from_secs(3))
            // Redirects stay on the plugin's own hosts.
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                let ok = attempt.previous().len() < 5 && allowed.allows(attempt.url());
                if ok {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .context("making the plugin's HTTP client")?;
        Ok(Plugin {
            id,
            manifest,
            config,
            engine,
            module,
            client,
            running: Arc::new(Semaphore::new(MAX_RUNNING)),
            fuel: FUEL,
            acts,
            annotates,
        })
    }

    /// Whether its results' buttons can be pressed.
    pub fn acts(&self) -> bool {
        self.acts
    }

    /// The results of `shown` it is shown: those about something it knows
    /// (`ids`), or all of them for a plugin without `ids`.
    fn picks_shown(&self, shown: &[ShownResult]) -> Vec<ShownResult> {
        shown
            .iter()
            .filter(|result| {
                self.manifest.ids.is_empty()
                    || result
                        .about
                        .as_ref()
                        .is_some_and(|about| self.manifest.knows(about))
            })
            .take(MAX_SHOWN)
            .cloned()
            .collect()
    }

    /// Runs the plugin's search on `query`, blocking; its fetches run on
    /// `runtime`.
    fn run(&self, query: &Query, runtime: &tokio::runtime::Handle) -> Result<Vec<Item>> {
        let deadline = Instant::now() + self.manifest.time();
        let output = self.call(
            "plumb_search",
            serde_json::to_vec(query)?,
            deadline,
            runtime,
        )?;
        Ok(output.results)
    }

    /// Calls the plugin's export `entry` with `input`, blocking, and
    /// reads what it handed back.
    fn call(
        &self,
        entry: &str,
        input: Vec<u8>,
        deadline: Instant,
        runtime: &tokio::runtime::Handle,
    ) -> Result<Output> {
        let host = Host {
            input,
            output: None,
            body: Vec::new(),
            headers: Vec::new(),
            status: 0,
            fetches: 0,
            deadline,
            limits: wasmi::StoreLimitsBuilder::new()
                .memory_size(MAX_MEMORY)
                .instances(1)
                .memories(1)
                .tables(4)
                .table_elements(100_000)
                .build(),
            manifest: self.manifest.clone(),
            client: self.client.clone(),
            runtime: runtime.clone(),
            id: self.id.clone(),
        };
        let mut store = Store::new(&self.engine, host);
        store.limiter(|host| &mut host.limits);
        store.set_fuel(self.fuel).map_err(wasm_error)?;
        let linker = linker(&self.engine)?;
        let instance = linker
            .instantiate_and_start(&mut store, &self.module)
            .map_err(wasm_error)?;
        let abi = instance
            .get_typed_func::<(), i32>(&store, "plumb_abi")
            .map_err(|_| {
                anyhow::anyhow!("plugin.wasm has no plumb_abi; build it with plumb-plugin")
            })?
            .call(&mut store, ())
            .map_err(wasm_error)?;
        if !(1..=plumb_plugin::ABI).contains(&abi) {
            bail!(
                "plugin.wasm is for plugin interface {abi}; this node runs 1 to {}",
                plumb_plugin::ABI
            );
        }
        instance
            .get_typed_func::<(), ()>(&store, entry)
            .map_err(|_| anyhow::anyhow!("plugin.wasm has no {entry}"))?
            .call(&mut store, ())
            .map_err(wasm_error)?;
        let Some(output) = store.into_data().output else {
            bail!("the plugin handed back nothing");
        };
        let output: Output =
            serde_json::from_slice(&output).context("reading what the plugin handed back")?;
        if let Some(error) = output.error {
            bail!("{error}");
        }
        Ok(output)
    }

    /// Fetches the pictures `items` name from the plugin's hosts and puts
    /// them in as `data:` URLs, so the page carries them; a picture that
    /// cannot be had by `deadline` is left out.
    fn fetch_images(
        &self,
        items: &mut [PluginItem],
        deadline: Instant,
        runtime: &tokio::runtime::Handle,
    ) {
        let wanted: Vec<(usize, url::Url)> = items
            .iter()
            .enumerate()
            .filter_map(|(at, item)| {
                let url = url::Url::parse(item.image.as_deref()?).ok()?;
                self.manifest.allows(&url).then_some((at, url))
            })
            .collect();
        for item in items.iter_mut() {
            item.image = None;
        }
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            return;
        };
        if wanted.is_empty() {
            return;
        }
        let client = self.client.clone();
        let fetched = runtime.block_on(async move {
            let mut fetches = tokio::task::JoinSet::new();
            for (at, url) in wanted {
                let client = client.clone();
                fetches.spawn(async move { (at, fetch_image(&client, url).await) });
            }
            let mut fetched = Vec::new();
            let _ = tokio::time::timeout(left, async {
                while let Some(Ok(done)) = fetches.join_next().await {
                    fetched.push(done);
                }
            })
            .await;
            fetched
        });
        for (at, image) in fetched {
            items[at].image = image;
        }
    }
}

/// The picture at `url` as a `data:` URL: PNG, JPEG, GIF or WebP, as its
/// bytes show, up to [`MAX_IMAGE`].
async fn fetch_image(client: &reqwest::Client, url: url::Url) -> Option<String> {
    let mut response = client.get(url).send().await.ok()?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|len| len > MAX_IMAGE as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > MAX_IMAGE {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    // Go by the bytes, not what the server says, so nothing but a picture
    // ends up in the page.
    let kind = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "gif"
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "webp"
    } else {
        return None;
    };
    Some(format!(
        "data:image/{kind};base64,{}",
        BASE64.encode(&bytes)
    ))
}

fn wasm_error(error: wasmi::Error) -> anyhow::Error {
    anyhow::anyhow!("the plugin failed: {error}")
}

/// What the sandbox keeps for one run.
struct Host {
    input: Vec<u8>,
    output: Option<Vec<u8>>,
    /// The last response's body, headers (as JSON) and status.
    body: Vec<u8>,
    headers: Vec<u8>,
    status: i32,
    fetches: usize,
    deadline: Instant,
    limits: StoreLimits,
    manifest: Manifest,
    client: reqwest::Client,
    runtime: tokio::runtime::Handle,
    id: String,
}

fn memory(caller: &Caller<'_, Host>) -> Result<Memory, wasmi::Error> {
    caller
        .get_export("memory")
        .and_then(Extern::into_memory)
        .ok_or_else(|| wasmi::Error::new("the plugin exports no memory"))
}

/// `len` bytes at `ptr` in the plugin's memory, at most `MAX_HANDED`.
fn read(caller: &Caller<'_, Host>, ptr: i32, len: i32) -> Result<Vec<u8>, wasmi::Error> {
    let len = usize::try_from(len).map_err(|_| wasmi::Error::new("a negative length"))?;
    if len > MAX_HANDED {
        return Err(wasmi::Error::new("too much handed over at once"));
    }
    let ptr = u32::try_from(ptr).map_err(|_| wasmi::Error::new("a bad pointer"))? as usize;
    let mut buffer = vec![0u8; len];
    memory(caller)?
        .read(caller, ptr, &mut buffer)
        .map_err(|e| wasmi::Error::new(e.to_string()))?;
    Ok(buffer)
}

fn write(caller: &mut Caller<'_, Host>, ptr: i32, bytes: &[u8]) -> Result<(), wasmi::Error> {
    let ptr = u32::try_from(ptr).map_err(|_| wasmi::Error::new("a bad pointer"))? as usize;
    let memory = memory(caller)?;
    memory
        .write(caller, ptr, bytes)
        .map_err(|e| wasmi::Error::new(e.to_string()))
}

fn linker(engine: &Engine) -> Result<Linker<Host>> {
    let mut linker = Linker::<Host>::new(engine);
    let defined = (|| -> Result<(), wasmi::Error> {
        linker.func_wrap("plumb", "input_len", |caller: Caller<'_, Host>| {
            caller.data().input.len() as i32
        })?;
        linker.func_wrap(
            "plumb",
            "input_read",
            |mut caller: Caller<'_, Host>, ptr: i32| {
                let input = std::mem::take(&mut caller.data_mut().input);
                let written = write(&mut caller, ptr, &input);
                caller.data_mut().input = input;
                written
            },
        )?;
        linker.func_wrap(
            "plumb",
            "fetch",
            |mut caller: Caller<'_, Host>, ptr: i32, len: i32| -> Result<i32, wasmi::Error> {
                let request = read(&caller, ptr, len)?;
                let host = caller.data_mut();
                host.body.clear();
                host.headers = b"[]".to_vec();
                host.status = 0;
                Ok(match host.fetch(&request) {
                    Ok((status, headers, body)) => {
                        host.status = i32::from(status);
                        host.headers = serde_json::to_vec(&headers).unwrap_or_default();
                        host.body = body;
                        host.body.len() as i32
                    }
                    Err(code) => code,
                })
            },
        )?;
        linker.func_wrap("plumb", "fetch_status", |caller: Caller<'_, Host>| {
            caller.data().status
        })?;
        linker.func_wrap(
            "plumb",
            "body_read",
            |mut caller: Caller<'_, Host>, ptr: i32| {
                let body = std::mem::take(&mut caller.data_mut().body);
                let written = write(&mut caller, ptr, &body);
                caller.data_mut().body = body;
                written
            },
        )?;
        linker.func_wrap("plumb", "headers_len", |caller: Caller<'_, Host>| {
            caller.data().headers.len() as i32
        })?;
        linker.func_wrap(
            "plumb",
            "headers_read",
            |mut caller: Caller<'_, Host>, ptr: i32| {
                let headers = std::mem::take(&mut caller.data_mut().headers);
                let written = write(&mut caller, ptr, &headers);
                caller.data_mut().headers = headers;
                written
            },
        )?;
        linker.func_wrap(
            "plumb",
            "output",
            |mut caller: Caller<'_, Host>, ptr: i32, len: i32| -> Result<(), wasmi::Error> {
                let output = read(&caller, ptr, len)?;
                caller.data_mut().output = Some(output);
                Ok(())
            },
        )?;
        linker.func_wrap(
            "plumb",
            "log",
            |caller: Caller<'_, Host>, ptr: i32, len: i32| -> Result<(), wasmi::Error> {
                let line = read(&caller, ptr, len.min(4096))?;
                // At debug level, on one line: a plugin sees the query, and
                // the node logs no queries by default.
                let line: String = String::from_utf8_lossy(&line)
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect();
                debug!("plugin {}: {line}", caller.data().id);
                Ok(())
            },
        )?;
        Ok(())
    })();
    defined.map_err(|e| anyhow::anyhow!("setting up the plugin sandbox: {e}"))?;
    Ok(linker)
}

/// A response's status, headers and body.
type Fetched = (u16, Vec<(String, String)>, Vec<u8>);

impl Host {
    /// Sends the plugin's request: its status, headers and body, or a
    /// `FETCH_*` code.
    fn fetch(&mut self, request: &[u8]) -> Result<Fetched, i32> {
        let Ok(request) = serde_json::from_slice::<Request>(request) else {
            return Err(plumb_plugin::FETCH_BAD_REQUEST);
        };
        let Ok(url) = url::Url::parse(&request.url) else {
            return Err(plumb_plugin::FETCH_BAD_REQUEST);
        };
        if !matches!(url.scheme(), "http" | "https") {
            return Err(plumb_plugin::FETCH_NOT_ALLOWED);
        }
        if !self.manifest.allows(&url) {
            return Err(plumb_plugin::FETCH_NOT_ALLOWED);
        }
        if self.fetches >= MAX_FETCHES {
            return Err(plumb_plugin::FETCH_TOO_MANY);
        }
        let Some(left) = self.deadline.checked_duration_since(Instant::now()) else {
            return Err(plumb_plugin::FETCH_TIME_UP);
        };
        let method = match request.method.to_ascii_uppercase().as_str() {
            "GET" => reqwest::Method::GET,
            "POST" => reqwest::Method::POST,
            "PUT" => reqwest::Method::PUT,
            "PATCH" => reqwest::Method::PATCH,
            "DELETE" => reqwest::Method::DELETE,
            "HEAD" => reqwest::Method::HEAD,
            _ => return Err(plumb_plugin::FETCH_BAD_REQUEST),
        };
        self.fetches += 1;
        let mut builder = self.client.request(method, url).timeout(left);
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        let id = &self.id;
        self.runtime.block_on(async move {
            let mut response = builder.send().await.map_err(|e| {
                debug!("plugin {id}: request failed: {e}");
                if e.is_timeout() {
                    plumb_plugin::FETCH_TIME_UP
                } else {
                    plumb_plugin::FETCH_FAILED
                }
            })?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .take(MAX_HEADERS)
                .map(|(name, value)| {
                    (
                        name.as_str().to_string(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect();
            if response
                .content_length()
                .is_some_and(|len| len > MAX_BODY as u64)
            {
                return Err(plumb_plugin::FETCH_TOO_LARGE);
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| plumb_plugin::FETCH_FAILED)?
            {
                if body.len() + chunk.len() > MAX_BODY {
                    return Err(plumb_plugin::FETCH_TOO_LARGE);
                }
                body.extend_from_slice(&chunk);
            }
            Ok((status, headers, body))
        })
    }
}

/// One plugin's results for a search.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PluginResults {
    /// The plugin's folder name.
    pub plugin: String,
    /// Its name, from `plugin.json`.
    pub name: String,
    pub results: Vec<PluginItem>,
}

/// One result from a plugin, checked and trimmed.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PluginItem {
    pub title: String,
    /// An `http`, `https` or `magnet` address.
    pub url: String,
    /// The site it is on; empty for a magnet link.
    pub site: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<u64>,
    /// Its picture, as a `data:` URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// A word or two shown beside the title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub badge: Option<String>,
    /// Its buttons, for the node's owner.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<PluginAction>,
}

/// A button on a plugin's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginAction {
    pub label: String,
    /// What the plugin's `plumb_act` gets, as JSON text.
    pub data: String,
}

/// What one plugin adds to one of the node's own results.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ResultNote {
    /// The plugin's folder name.
    pub plugin: String,
    /// Its name, from `plugin.json`.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub badge: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<PluginAction>,
    /// The plugin leaves the result off the page.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hide: bool,
}

/// Plugins' notes on the node's results, by the results' addresses.
pub type ResultNotes = HashMap<String, Vec<ResultNote>>;

/// The buttons kept of `actions`: labelled, with little data, at most
/// [`MAX_ACTIONS`]; none for a plugin that cannot `act`.
fn clean_actions(actions: Vec<plumb_plugin::Action>, acts: bool) -> Vec<PluginAction> {
    if !acts {
        return Vec::new();
    }
    actions
        .into_iter()
        .filter_map(|action| {
            let label = squash(&action.label, 30);
            let data = serde_json::to_string(&action.data).ok()?;
            (!label.is_empty() && data.len() <= MAX_ACTION_DATA)
                .then_some(PluginAction { label, data })
        })
        .take(MAX_ACTIONS)
        .collect()
}

/// Keeps the notes of `notes` about results of `shown`, checked, by the
/// results' addresses; notes that say nothing are left out.
fn clean_notes(
    notes: Vec<Note>,
    shown: &[ShownResult],
    plugin: &Plugin,
) -> Vec<(String, ResultNote)> {
    let mut kept: Vec<(String, ResultNote)> = Vec::new();
    for note in notes {
        let Some(result) = shown.iter().find(|r| r.id == note.id) else {
            continue;
        };
        if kept.iter().any(|(url, _)| *url == result.url) {
            continue;
        }
        let kept_note = ResultNote {
            plugin: plugin.id.clone(),
            name: plugin.manifest.name.clone(),
            badge: note.badge.map(|b| squash(&b, 24)).filter(|b| !b.is_empty()),
            actions: clean_actions(note.actions, plugin.acts),
            hide: note.hide,
        };
        if kept_note.badge.is_some() || !kept_note.actions.is_empty() || kept_note.hide {
            kept.push((result.url.clone(), kept_note));
        }
    }
    kept
}

/// Whether `url` is a magnet link the page can offer: `magnet:?` with an
/// exact topic.
fn is_magnet(url: &str) -> bool {
    url.strip_prefix("magnet:?").is_some_and(|rest| {
        rest.split('&').any(|part| part.starts_with("xt="))
            && !rest.chars().any(|c| c.is_whitespace() || c.is_control())
    })
}

/// Keeps what a plugin said that a results page can show: `http(s)` and
/// magnet links with titles, trimmed, without what safe search leaves
/// out; buttons only for a plugin that `acts`. Pictures are still their
/// addresses, for [`Plugin::fetch_images`].
fn clean(items: Vec<Item>, safe: SafeSearch, acts: bool) -> Vec<PluginItem> {
    let mut kept = Vec::new();
    for item in items {
        let raw = item.url.trim();
        if raw.len() > 2048 {
            continue;
        }
        let (url, site) = if is_magnet(raw) {
            (raw.to_string(), String::new())
        } else {
            let Ok(url) = url::Url::parse(raw) else {
                continue;
            };
            if !matches!(url.scheme(), "http" | "https") {
                continue;
            }
            let host = url.host_str().unwrap_or_default().to_string();
            let site = plumb_core::registrable_domain(&host).unwrap_or(host);
            (url.to_string(), site)
        };
        let title = squash(&item.title, 200);
        if title.is_empty() {
            continue;
        }
        let snippet = item
            .snippet
            .map(|s| squash(&s, 400))
            .filter(|s| !s.is_empty());
        let badge = item.badge.map(|b| squash(&b, 24)).filter(|b| !b.is_empty());
        let level = plumb_core::safe::adult_level(
            &site,
            std::iter::once(title.as_str())
                .chain(snippet.as_deref())
                .chain(badge.as_deref()),
        );
        if safe.hides(level) {
            continue;
        }
        let actions = clean_actions(item.actions, acts);
        kept.push(PluginItem {
            title,
            url,
            site,
            snippet,
            published: item.published,
            image: item
                .image
                .filter(|i| i.starts_with("http://") || i.starts_with("https://")),
            badge,
            actions,
        });
        if kept.len() == MAX_RESULTS {
            break;
        }
    }
    kept
}

/// `text` on one line, at most `max` characters.
fn squash(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    plumb_core::truncate_chars(&line, max)
}

type CacheKey = (String, String);

/// One plugin's notes on a page's results, with the results' addresses.
type KeptNotes = Vec<(String, ResultNote)>;

/// A node's plugins. Cheap to clone.
#[derive(Clone, Default)]
pub struct Plugins {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    plugins: Vec<Arc<Plugin>>,
    /// Plugins' results, by plugin and what it was asked, with when they
    /// stop being reused.
    cache: Mutex<HashMap<CacheKey, (Instant, Vec<PluginItem>)>>,
    /// Plugins' notes on the node's results, by the same keys.
    notes_cache: Mutex<HashMap<CacheKey, (Instant, KeptNotes)>>,
    /// Goes in the forms of the buttons on this node's results pages, so
    /// that a page of another site cannot press them.
    token: String,
    /// The owner's choices of what a search that fits a plugin does, by
    /// folder name, and the file they are kept in.
    suggest: Mutex<BTreeMap<String, Suggest>>,
    suggest_file: Option<PathBuf>,
}

impl std::fmt::Debug for Plugins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.inner.plugins.iter()).finish()
    }
}

impl PartialEq for Plugins {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for Plugins {}

/// The plugin, and what it is asked, for one search.
/// A plugin to run: where it was installed, it, what it is asked, and
/// whether it runs for what the search is about by one of its `run_ids`,
/// so that its results are kept for that thing, whatever words found it.
type Run = (usize, Arc<Plugin>, Query, bool);

impl Plugins {
    pub fn new(plugins: Vec<Plugin>) -> Self {
        let mut token = [0u8; 16];
        if getrandom::fill(&mut token).is_err() {
            warn!("no randomness for the plugin buttons' token; their buttons are off");
        }
        let token = if token == [0u8; 16] {
            String::new()
        } else {
            token.iter().map(|b| format!("{b:02x}")).collect()
        };
        Plugins {
            inner: Arc::new(Inner {
                plugins: plugins.into_iter().map(Arc::new).collect(),
                cache: Mutex::default(),
                notes_cache: Mutex::default(),
                token,
                suggest: Mutex::default(),
                suggest_file: None,
            }),
        }
    }

    /// These plugins, with the owner's choices kept in `file`
    /// ([`SUGGEST_FILE`] in the plugins' folder).
    fn with_suggest_file(self, file: PathBuf) -> Self {
        let chosen: BTreeMap<String, Suggest> = match std::fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                warn!("{} left out: {error}", file.display());
                BTreeMap::new()
            }),
            Err(_) => BTreeMap::new(),
        };
        let inner = Arc::into_inner(self.inner).expect("not shared yet");
        Plugins {
            inner: Arc::new(Inner {
                suggest: Mutex::new(chosen),
                suggest_file: Some(file),
                ..inner
            }),
        }
    }

    /// What a search that fits `plugin` does: the owner's choice, or its
    /// `plugin.json`'s.
    pub fn suggest(&self, plugin: &Plugin) -> Suggest {
        let chosen = self.inner.suggest.lock().expect("suggest lock");
        chosen
            .get(&plugin.id)
            .copied()
            .unwrap_or(plugin.manifest.suggest)
    }

    /// Keeps the owner's choice of what a search that fits the plugin in
    /// folder `id` does; from the next search on.
    pub fn set_suggest(&self, id: &str, suggest: Suggest) -> Result<()> {
        let Some(plugin) = self.inner.plugins.iter().find(|p| p.id == id) else {
            bail!("no plugin {id:?} on this node");
        };
        let mut chosen = self.inner.suggest.lock().expect("suggest lock");
        if suggest == plugin.manifest.suggest {
            chosen.remove(id);
        } else {
            chosen.insert(id.to_string(), suggest);
        }
        if let Some(file) = &self.inner.suggest_file {
            let json = serde_json::to_vec_pretty(&*chosen)?;
            let part = file.with_extension("json.part");
            std::fs::write(&part, json).with_context(|| format!("writing {}", part.display()))?;
            std::fs::rename(&part, file).with_context(|| format!("writing {}", file.display()))?;
        }
        Ok(())
    }

    /// Links to the plugins a search for `query`, taken to be `about`
    /// one thing, fits but does not run, by the owner's choice; never to
    /// the plugin the search was asked to `run`.
    pub fn offers(&self, query: &str, about: Option<&About>, run: Option<&str>) -> Vec<Offer> {
        if query.trim().is_empty() {
            return Vec::new();
        }
        self.inner
            .plugins
            .iter()
            .filter(|plugin| run != Some(plugin.id.as_str()))
            .filter(|plugin| {
                plugin.manifest.picks(query, about, self.suggest(plugin)) == Some(Pick::Offer)
            })
            .map(|plugin| Offer {
                plugin: plugin.id.clone(),
                name: plugin.manifest.name.clone(),
            })
            .collect()
    }

    /// The plugins in the folders of `dir`; none when it does not exist.
    /// A plugin that cannot load is left out, with a warning.
    pub fn load_dir(dir: &Path) -> Self {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Plugins::default();
        };
        let mut folders: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.is_dir())
            .collect();
        folders.sort();
        let mut plugins = Vec::new();
        for folder in folders {
            match Plugin::load(&folder) {
                Ok(plugin) => {
                    info!(
                        "plugin {} ({}) loaded; it may fetch from {}{}",
                        plugin.id,
                        plugin.manifest.name,
                        plugin.manifest.hosts.join(", "),
                        if plugin.acts { "; it has buttons" } else { "" }
                    );
                    plugins.push(plugin);
                }
                Err(error) => warn!("plugin in {} left out: {error:#}", folder.display()),
            }
        }
        Plugins::new(plugins).with_suggest_file(dir.join(SUGGEST_FILE))
    }

    pub fn is_empty(&self) -> bool {
        self.inner.plugins.is_empty()
    }

    pub fn list(&self) -> impl Iterator<Item = &Plugin> {
        self.inner.plugins.iter().map(Arc::as_ref)
    }

    /// Whether any plugin has buttons.
    pub fn any_act(&self) -> bool {
        self.inner.plugins.iter().any(|p| p.acts)
    }

    /// The token the buttons' forms carry; empty when buttons are off.
    pub fn token(&self) -> &str {
        &self.inner.token
    }

    /// Whether `token` is this node's buttons' token.
    pub fn accepts(&self, token: &str) -> bool {
        use subtle::ConstantTimeEq as _;
        let own = self.inner.token.as_bytes();
        !own.is_empty() && own.len() == token.len() && bool::from(own.ct_eq(token.as_bytes()))
    }

    /// The results of the plugins `query` picks, by keyword or because of
    /// what it is `about`, in the order they were installed; plugins that
    /// fail, are busy or take too long are left out. Waits at most a few
    /// seconds.
    pub async fn search(
        &self,
        query: &str,
        safe: SafeSearch,
        language: Option<&str>,
        about: Option<&About>,
    ) -> Vec<PluginResults> {
        self.search_running(query, safe, language, about, None)
            .await
    }

    /// [`Plugins::search`], also running the plugin in folder `run`
    /// on the whole query when its keywords do not: an offer's link.
    pub async fn search_running(
        &self,
        query: &str,
        safe: SafeSearch,
        language: Option<&str>,
        about: Option<&About>,
        run: Option<&str>,
    ) -> Vec<PluginResults> {
        if self.is_empty() || query.trim().is_empty() {
            return Vec::new();
        }
        let runs = self
            .inner
            .plugins
            .iter()
            .enumerate()
            .filter_map(|(at, plugin)| {
                let asked = run == Some(plugin.id.as_str());
                let suggest = if asked {
                    Suggest::Automatic
                } else {
                    self.suggest(plugin)
                };
                let (keyword, terms) = match plugin.manifest.picks(query, about, suggest) {
                    Some(Pick::Run(keyword, terms)) => (keyword, terms),
                    _ if asked => (None, query.split_whitespace().collect::<Vec<_>>().join(" ")),
                    _ => return None,
                };
                let for_thing =
                    keyword.is_none() && about.is_some_and(|a| plugin.manifest.surely_for(a));
                let input = Query {
                    text: query.to_string(),
                    terms,
                    keyword,
                    safe: safe.as_str().to_string(),
                    language: language.map(str::to_string),
                    config: plugin.config.clone(),
                    about: about.cloned(),
                    page: None,
                };
                Some((at, Arc::clone(plugin), input, for_thing))
            })
            .collect();
        self.gather(runs, safe).await
    }

    /// What the plugins that know the site of the page at `url` say about
    /// it, for a browser extension showing them beside that page.
    pub async fn page(
        &self,
        url: &str,
        safe: SafeSearch,
        language: Option<&str>,
    ) -> Vec<PluginResults> {
        let Ok(parsed) = url::Url::parse(url) else {
            return Vec::new();
        };
        if !matches!(parsed.scheme(), "http" | "https") {
            return Vec::new();
        }
        let runs = self
            .inner
            .plugins
            .iter()
            .enumerate()
            .filter(|(_, plugin)| plugin.manifest.covers_page(&parsed))
            .map(|(at, plugin)| {
                let input = Query {
                    text: url.to_string(),
                    terms: String::new(),
                    keyword: None,
                    safe: safe.as_str().to_string(),
                    language: language.map(str::to_string),
                    config: plugin.config.clone(),
                    about: None,
                    page: Some(url.to_string()),
                };
                (at, Arc::clone(plugin), input, false)
            })
            .collect();
        self.gather(runs, safe).await
    }

    /// Runs `runs` side by side, each with its own time, and keeps what
    /// they find.
    async fn gather(&self, runs: Vec<Run>, safe: SafeSearch) -> Vec<PluginResults> {
        let runtime = tokio::runtime::Handle::current();
        let mut running = tokio::task::JoinSet::new();
        let mut found: Vec<(usize, PluginResults)> = Vec::new();
        let mut longest = Duration::ZERO;
        for (at, plugin, input, for_thing) in runs {
            let mut ttl = plugin.manifest.cache_time();
            let asked = if for_thing {
                // A song's results are the same for "creep" and "radiohead
                // creep", and for a day: each song uses the quota once.
                if !ttl.is_zero() {
                    ttl = ttl.max(THING_CACHE_TIME);
                }
                Query {
                    text: String::new(),
                    terms: String::new(),
                    ..input.clone()
                }
            } else {
                input.clone()
            };
            let key = (
                plugin.id.clone(),
                serde_json::to_string(&asked).unwrap_or_default(),
            );
            if let Some(results) = self.cached(&key) {
                found.push((at, plugin.results(results)));
                continue;
            }
            let Ok(permit) = Arc::clone(&plugin.running).try_acquire_owned() else {
                debug!("plugin {} is busy; left out", plugin.id);
                continue;
            };
            longest = longest.max(plugin.manifest.time());
            let runtime = runtime.clone();
            running.spawn_blocking(move || {
                let _permit = permit;
                let deadline = Instant::now() + plugin.manifest.time();
                let ran = plugin.run(&input, &runtime).map(|items| {
                    let mut items = clean(items, safe, plugin.acts);
                    plugin.fetch_images(&mut items, deadline, &runtime);
                    items
                });
                (at, plugin, key, ttl, ran)
            });
        }
        let waited = tokio::time::timeout(longest + Duration::from_millis(500), async {
            while let Some(done) = running.join_next().await {
                let Ok((at, plugin, key, ttl, ran)) = done else {
                    continue;
                };
                match ran {
                    Ok(items) => {
                        if !ttl.is_zero() {
                            self.keep(key, items.clone(), ttl);
                        }
                        found.push((at, plugin.results(items)));
                    }
                    Err(error) => warn!("plugin {} found nothing: {error:#}", plugin.id),
                }
            }
        })
        .await;
        if waited.is_err() {
            // The sandbox stops on its own: its fetches time out with
            // the search, and its fuel runs out.
            warn!("plugins took too long; their results are left out");
            running.detach_all();
        }
        found.sort_by_key(|(at, _)| *at);
        found
            .into_iter()
            .map(|(_, results)| results)
            .filter(|r| !r.results.is_empty())
            .collect()
    }

    /// What the plugins that mark up results say about `shown`, the
    /// node's own results for `query`: badges, buttons and results to
    /// leave out, by the results' addresses. Plugins run side by side,
    /// each with its own time; one that fails or is late adds nothing.
    pub async fn annotate(&self, query: &str, shown: &[ShownResult]) -> ResultNotes {
        let runtime = tokio::runtime::Handle::current();
        let mut running = tokio::task::JoinSet::new();
        let mut found: Vec<(usize, Vec<(String, ResultNote)>)> = Vec::new();
        let mut longest = Duration::ZERO;
        for (at, plugin) in self.inner.plugins.iter().enumerate() {
            if !plugin.annotates {
                continue;
            }
            let picked = plugin.picks_shown(shown);
            if picked.is_empty() {
                continue;
            }
            let input = Shown {
                query: query.to_string(),
                results: picked,
                config: plugin.config.clone(),
            };
            let Ok(json) = serde_json::to_vec(&input) else {
                continue;
            };
            let key = (
                plugin.id.clone(),
                String::from_utf8_lossy(&json).into_owned(),
            );
            let ttl = plugin.manifest.cache_time();
            let cached = self.inner.notes_cache.lock().ok().and_then(|cache| {
                let (when, notes) = cache.get(&key)?;
                (when.elapsed() < ttl).then(|| notes.clone())
            });
            if let Some(notes) = cached {
                found.push((at, notes));
                continue;
            }
            let Ok(permit) = Arc::clone(&plugin.running).try_acquire_owned() else {
                debug!("plugin {} is busy; left out", plugin.id);
                continue;
            };
            longest = longest.max(plugin.manifest.time());
            let plugin = Arc::clone(plugin);
            let runtime = runtime.clone();
            running.spawn_blocking(move || {
                let _permit = permit;
                let deadline = Instant::now() + plugin.manifest.time();
                let ran = plugin
                    .call("plumb_annotate", json, deadline, &runtime)
                    .map(|output| clean_notes(output.notes, &input.results, &plugin));
                (at, plugin, key, ran)
            });
        }
        let waited = tokio::time::timeout(longest + Duration::from_millis(500), async {
            while let Some(done) = running.join_next().await {
                let Ok((at, plugin, key, ran)) = done else {
                    continue;
                };
                match ran {
                    Ok(notes) => {
                        let ttl = plugin.manifest.cache_time();
                        if !ttl.is_zero() {
                            if let Ok(mut cache) = self.inner.notes_cache.lock() {
                                if cache.len() >= CACHE_ENTRIES {
                                    cache.clear();
                                }
                                cache.insert(key, (Instant::now(), notes.clone()));
                            }
                        }
                        found.push((at, notes));
                    }
                    Err(error) => warn!("plugin {} marked up nothing: {error:#}", plugin.id),
                }
            }
        })
        .await;
        if waited.is_err() {
            warn!("plugins took too long marking up results; left out");
            running.detach_all();
        }
        found.sort_by_key(|(at, _)| *at);
        let mut notes = ResultNotes::new();
        for (url, note) in found.into_iter().flat_map(|(_, notes)| notes) {
            notes.entry(url).or_default().push(note);
        }
        notes
    }

    /// Whether any plugin marks up the node's results.
    pub fn any_annotate(&self) -> bool {
        self.inner.plugins.iter().any(|p| p.annotates)
    }

    /// Presses a button of plugin `id`: runs its `plumb_act` with `data`
    /// (JSON text) and returns the line it hands back for the owner. Its
    /// saved results are dropped, since what it shows may have changed.
    pub async fn act(&self, id: &str, data: &str) -> Result<String> {
        let plugin = self
            .inner
            .plugins
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .with_context(|| format!("no plugin {id:?} on this node"))?;
        if !plugin.acts {
            bail!("plugin {id:?} has no buttons");
        }
        if data.len() > MAX_ACTION_DATA {
            bail!("the button's data is too big");
        }
        let input = ActInput {
            data: serde_json::from_str(data).context("reading the button's data")?,
            config: plugin.config.clone(),
        };
        let input = serde_json::to_vec(&input)?;
        let runtime = tokio::runtime::Handle::current();
        let ran = Arc::clone(&plugin);
        let output = tokio::task::spawn_blocking(move || {
            ran.call("plumb_act", input, Instant::now() + ACT_TIME, &runtime)
        })
        .await
        .context("the plugin's action failed")??;
        self.forget(&plugin.id);
        Ok(squash(output.message.as_deref().unwrap_or("Done."), 300))
    }

    fn cached(&self, key: &CacheKey) -> Option<Vec<PluginItem>> {
        let cache = self.inner.cache.lock().ok()?;
        let (until, items) = cache.get(key)?;
        (Instant::now() < *until).then(|| items.clone())
    }

    fn keep(&self, key: CacheKey, items: Vec<PluginItem>, ttl: Duration) {
        let Ok(mut cache) = self.inner.cache.lock() else {
            return;
        };
        let now = Instant::now();
        if cache.len() >= CACHE_ENTRIES {
            cache.retain(|_, (until, _)| now < *until);
        }
        if cache.len() >= CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(key, (now + ttl, items));
    }

    /// Drops the saved results and notes of plugin `id`.
    fn forget(&self, id: &str) {
        if let Ok(mut cache) = self.inner.cache.lock() {
            cache.retain(|(plugin, _), _| plugin != id);
        }
        if let Ok(mut cache) = self.inner.notes_cache.lock() {
            cache.retain(|(plugin, _), _| plugin != id);
        }
    }
}

impl Plugin {
    fn results(&self, results: Vec<PluginItem>) -> PluginResults {
        PluginResults {
            plugin: self.id.clone(),
            name: self.manifest.name.clone(),
            results,
        }
    }

    /// Runs the plugin on `query` as typed, whether or not its keywords
    /// pick it: for `plumb try-plugin`. Its results, checked as a search
    /// would check them, or why it found nothing.
    pub async fn try_query(self: Arc<Self>, query: &str) -> Result<Vec<PluginItem>> {
        let (keyword, terms) = match self.manifest.picks(query, None, Suggest::Automatic) {
            Some(Pick::Run(keyword, terms)) => (keyword, terms),
            _ => (None, query.to_string()),
        };
        let page = url::Url::parse(query)
            .ok()
            .filter(|url| self.manifest.covers_page(url))
            .map(|_| query.to_string());
        let input = Query {
            text: query.to_string(),
            terms: if page.is_some() { String::new() } else { terms },
            keyword,
            safe: SafeSearch::Moderate.as_str().to_string(),
            language: None,
            config: self.config.clone(),
            about: None,
            page,
        };
        let runtime = tokio::runtime::Handle::current();
        let items = tokio::task::spawn_blocking(move || {
            let deadline = Instant::now() + self.manifest.time();
            self.run(&input, &runtime).map(|items| {
                let mut items = clean(items, SafeSearch::Moderate, self.acts);
                self.fetch_images(&mut items, deadline, &runtime);
                items
            })
        })
        .await
        .context("the plugin's run failed")??;
        Ok(items)
    }
}

/// `plumb try-plugin`: runs the plugin in folder `dir` on `query` and
/// prints its results as JSON; with `act`, presses a button with that
/// data instead and prints what the plugin said; with `annotate`, a file
/// of results as a node shows them, prints its notes on them.
pub fn try_plugin(
    dir: &Path,
    query: &str,
    act: Option<&str>,
    annotate: Option<&Path>,
) -> Result<()> {
    let plugin = Plugin::load(dir)?;
    let runtime = tokio::runtime::Runtime::new().context("starting the runtime")?;
    if let Some(data) = act {
        let plugins = Plugins::new(vec![plugin]);
        let id = plugins
            .list()
            .next()
            .map(|p| p.id.clone())
            .unwrap_or_default();
        let message = runtime.block_on(plugins.act(&id, data))?;
        println!("{message}");
        return Ok(());
    }
    if let Some(path) = annotate {
        if !plugin.annotates {
            bail!("the plugin does not mark up results (it has no plumb_annotate)");
        }
        let shown: Vec<ShownResult> = serde_json::from_slice(
            &std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
        )
        .with_context(|| format!("reading {}", path.display()))?;
        let plugins = Plugins::new(vec![plugin]);
        let notes = runtime.block_on(plugins.annotate(query, &shown));
        println!("{}", serde_json::to_string_pretty(&notes)?);
        return Ok(());
    }
    let items = runtime.block_on(Arc::new(plugin).try_query(query))?;
    println!("{}", serde_json::to_string_pretty(&items)?);
    Ok(())
}

/// What a results page knows a search is about, for plugins: the
/// Wikipedia article or Wikidata item `page`, its item and its profiles'
/// identifiers; or the song or album `page` of the music set, by its
/// MusicBrainz identifier (`musicbrainz-recording` for a song,
/// `musicbrainz-album` for an album) and its artist.
pub fn about_page(page: &plumb_index::pages::Page) -> About {
    let mut ids = BTreeMap::new();
    for profile in &page.profiles {
        ids.entry(profile.service.clone())
            .or_insert_with(|| profile.id.clone());
    }
    let mut by = None;
    if page.set == plumb_index::pages::MUSIC_SET {
        let mbid = |kind: &str| {
            page.url
                .strip_prefix("https://musicbrainz.org/")
                .and_then(|rest| rest.strip_prefix(kind))
                .and_then(|rest| rest.strip_prefix('/'))
                .map(str::to_string)
        };
        if let Some(mbid) = mbid("recording") {
            ids.insert("musicbrainz-recording".into(), mbid);
        } else if let Some(mbid) = mbid("release-group") {
            ids.insert("musicbrainz-album".into(), mbid);
        }
        by = page.description.as_deref().and_then(music_artist);
    }
    About {
        title: page.title.clone(),
        description: page.description.clone().filter(|d| !d.trim().is_empty()),
        wikidata: page.item.clone(),
        ids,
        by,
    }
}

/// The artist in a music page's description: "Queen" for "Song by Queen,
/// 1975" or "Album by Queen · Rock"; a comma that is not before the year
/// stays ("Crosby, Stills, Nash & Young").
fn music_artist(description: &str) -> Option<String> {
    let by = description
        .strip_prefix("Song by ")
        .or_else(|| description.strip_prefix("Album by "))?;
    let by = by.split(" · ").next().unwrap_or(by);
    let by = match by.rsplit_once(", ") {
        Some((artist, year)) if !year.is_empty() && year.bytes().all(|b| b.is_ascii_digit()) => {
            artist
        }
        _ => by,
    };
    let by = by.trim();
    (!by.is_empty()).then(|| by.to_string())
}

/// A plugin module (WebAssembly text) that hands back `output`, JSON,
/// whatever it is asked.
#[cfg(test)]
pub(crate) fn answering(output: &str) -> String {
    let escaped: String = output.bytes().map(|b| format!("\\{b:02x}")).collect();
    format!(
        r#"(module
            (import "plumb" "output" (func $output (param i32 i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "{escaped}")
            (func (export "plumb_abi") (result i32) (i32.const 1))
            (func (export "plumb_search") (call $output (i32.const 0) (i32.const {len})))
            (func (export "plumb_act") (call $output (i32.const 0) (i32.const {len}))))"#,
        len = output.len()
    )
}

/// A plugin module that only marks up results, handing back `output`
/// whatever it is shown.
#[cfg(test)]
pub(crate) fn annotating(output: &str) -> String {
    let escaped: String = output.bytes().map(|b| format!("\\{b:02x}")).collect();
    format!(
        r#"(module
            (import "plumb" "output" (func $output (param i32 i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "{escaped}")
            (func (export "plumb_abi") (result i32) (i32.const 2))
            (func (export "plumb_annotate") (call $output (i32.const 0) (i32.const {len}))))"#,
        len = output.len()
    )
}

/// Plugins of one plugin, named `name`, that marks up results with
/// `output`.
#[cfg(test)]
pub(crate) fn annotating_plugins(name: &str, output: &str) -> Plugins {
    let manifest = Manifest {
        name: name.into(),
        hosts: vec!["a.example".into()],
        ..Manifest::default()
    };
    let plugin = Plugin::from_parts(
        name.to_lowercase().replace(' ', "-"),
        manifest,
        serde_json::Value::Null,
        annotating(output).as_bytes(),
    )
    .expect("a plugin");
    Plugins::new(vec![plugin])
}

/// Plugins of one plugin, named `name`, that `keyword` runs and that
/// hands back `output`.
#[cfg(test)]
pub(crate) fn answering_plugins(name: &str, keyword: &str, output: &str) -> Plugins {
    let manifest = Manifest {
        name: name.into(),
        hosts: vec!["a.example".into()],
        keywords: vec![keyword.into()],
        ..Manifest::default()
    };
    let plugin = Plugin::from_parts(
        name.to_lowercase().replace(' ', "-"),
        manifest,
        serde_json::Value::Null,
        answering(output).as_bytes(),
    )
    .expect("a plugin");
    Plugins::new(vec![plugin])
}

/// [`answering_plugins`], for a plugin that searches with `hint` fit,
/// with a link to its results rather than running it.
#[cfg(test)]
pub(crate) fn offering_plugins(name: &str, hint: &str, output: &str) -> Plugins {
    let manifest = Manifest {
        name: name.into(),
        hosts: vec!["a.example".into()],
        keywords: vec!["x".into()],
        hints: vec![hint.into()],
        suggest: Suggest::Button,
        ..Manifest::default()
    };
    let plugin = Plugin::from_parts(
        name.to_lowercase().replace(' ', "-"),
        manifest,
        serde_json::Value::Null,
        answering(output).as_bytes(),
    )
    .expect("a plugin");
    Plugins::new(vec![plugin])
}

#[cfg(test)]
mod tests;
