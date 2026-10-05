//! Plugins: results from sources Plumb does not crawl, added by a node's
//! owner on their own node.
//!
//! A plugin is a folder in the data folder's `plugins/` with a
//! `plugin.json` (its name, the hosts it may fetch from and the keywords
//! that run it), a `plugin.wasm` built with the `plumb-plugin` crate, and
//! optionally a `config.json` of the owner's settings for it. Nodes come
//! with none, and plumbsearch.org runs none.
//!
//! Each plugin runs in a WebAssembly sandbox (wasmi) with a fuel and
//! memory limit. It can read the query and fetch from its own hosts, a
//! few requests per search, and nothing else: no files, no other
//! connections, nothing about who searched. Its results show only on
//! this node's own pages and APIs, and are never shared with other
//! nodes or kept in records.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use plumb_core::SafeSearch;
use plumb_plugin::{Item, Output, Query, Request};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};
use wasmi::{Caller, Config, Engine, Extern, Linker, Memory, Module, Store, StoreLimits};

/// The folder in a node's data folder that holds its plugins.
pub const PLUGINS_DIR: &str = "plugins";

/// How long a search waits for its plugins.
const SEARCH_TIME: Duration = Duration::from_secs(4);
/// Requests one plugin may make for one search.
const MAX_FETCHES: usize = 4;
/// The biggest response a plugin may read.
const MAX_BODY: usize = 2 * 1024 * 1024;
/// The biggest request, output or log line a plugin may hand over.
const MAX_HANDED: usize = 1024 * 1024;
/// A plugin's memory.
const MAX_MEMORY: usize = 64 * 1024 * 1024;
/// About this many WebAssembly instructions per search: plenty to read a
/// few API answers, not enough to keep a thread busy for long.
const FUEL: u64 = 2_000_000_000;
/// Results kept from one plugin for one search.
const MAX_RESULTS: usize = 10;
/// Searches one plugin runs at once; more are left without its results.
const MAX_RUNNING: usize = 4;
/// How long a plugin's results for a query are reused, which keeps a
/// busy node from asking its sources the same thing over and over.
const CACHE_TIME: Duration = Duration::from_secs(600);
const CACHE_ENTRIES: usize = 1000;

/// A plugin's `plugin.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Manifest {
    /// What the results page calls it: "From Hacker News".
    pub name: String,
    /// A line about where its results come from.
    #[serde(default)]
    pub about: String,
    /// The hosts it may fetch from: `api.example.org`, or
    /// `*.example.org` for every subdomain.
    pub hosts: Vec<String>,
    /// Words or phrases that run it when a query starts or ends with
    /// one ("hn", "hacker news"); they are taken off what it searches.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Runs it for every search. Mind the source's limits.
    #[serde(default)]
    pub always: bool,
}

impl Manifest {
    fn check(&self) -> Result<()> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 60 {
            bail!("its name must be 1 to 60 characters");
        }
        if self.keywords.iter().all(|k| k.trim().is_empty()) && !self.always {
            bail!("it needs keywords, or \"always\": true");
        }
        for host in &self.hosts {
            let bare = host.strip_prefix("*.").unwrap_or(host);
            if bare.is_empty()
                || bare.contains(['/', ':', '*', ' ', '@'])
                || !bare.contains('.') && bare != "localhost"
            {
                bail!("{host:?} is not a host name such as api.example.org or *.example.org");
            }
        }
        Ok(())
    }

    /// Whether the plugin may fetch from `host`.
    fn allows(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.hosts.iter().any(|allowed| {
            let allowed = allowed.to_ascii_lowercase();
            match allowed.strip_prefix("*.") {
                Some(parent) => host
                    .strip_suffix(parent)
                    .is_some_and(|sub| sub.ends_with('.') && sub.len() > 1),
                None => host == allowed,
            }
        })
    }

    /// The keyword `query` starts or ends with, and the rest of it; for
    /// a plugin that runs on every search, the whole query.
    fn picks(&self, query: &str) -> Option<(Option<String>, String)> {
        let words: Vec<&str> = query.split_whitespace().collect();
        let lower: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
        for keyword in &self.keywords {
            let wanted: Vec<String> = keyword.split_whitespace().map(str::to_lowercase).collect();
            let n = wanted.len();
            if n == 0 || n >= words.len() {
                continue;
            }
            if lower[..n] == wanted[..] {
                return Some((Some(keyword.clone()), words[n..].join(" ")));
            }
            if lower[words.len() - n..] == wanted[..] {
                return Some((Some(keyword.clone()), words[..words.len() - n].join(" ")));
            }
        }
        self.always.then(|| (None, words.join(" ")))
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
        manifest
            .check()
            .with_context(|| format!("checking {}", manifest_path.display()))?;
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
    }

    /// A plugin from its manifest, settings and module (binary or text).
    pub fn from_parts(
        id: String,
        manifest: Manifest,
        config: serde_json::Value,
        wasm: &[u8],
    ) -> Result<Self> {
        manifest.check()?;
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
        let allowed = manifest.clone();
        let client = reqwest::Client::builder()
            .user_agent(format!(
                "PlumbSearch/{} plugin {id} (+https://github.com/SueHeir/plumb-search)",
                env!("CARGO_PKG_VERSION")
            ))
            .connect_timeout(Duration::from_secs(3))
            // Redirects stay on the plugin's own hosts.
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                let ok = attempt.previous().len() < 5
                    && attempt.url().host_str().is_some_and(|h| allowed.allows(h));
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
        })
    }

    /// Runs the plugin on `query`, blocking; its fetches run on `runtime`.
    fn run(&self, query: &Query, runtime: &tokio::runtime::Handle) -> Result<Vec<Item>> {
        let input = serde_json::to_vec(query)?;
        let host = Host {
            input,
            output: None,
            body: Vec::new(),
            status: 0,
            fetches: 0,
            deadline: Instant::now() + SEARCH_TIME,
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
        if abi != plumb_plugin::ABI {
            bail!(
                "plugin.wasm is for plugin interface {abi}; this node runs {}",
                plumb_plugin::ABI
            );
        }
        instance
            .get_typed_func::<(), ()>(&store, "plumb_search")
            .map_err(|_| anyhow::anyhow!("plugin.wasm has no plumb_search"))?
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
        Ok(output.results)
    }
}

fn wasm_error(error: wasmi::Error) -> anyhow::Error {
    anyhow::anyhow!("the plugin failed: {error}")
}

/// What the sandbox keeps for one run.
struct Host {
    input: Vec<u8>,
    output: Option<Vec<u8>>,
    /// The last response's body and status.
    body: Vec<u8>,
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
                host.status = 0;
                Ok(match host.fetch(&request) {
                    Ok((status, body)) => {
                        host.status = i32::from(status);
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

impl Host {
    /// Sends the plugin's request: its status and body, or a `FETCH_*`
    /// code.
    fn fetch(&mut self, request: &[u8]) -> Result<(u16, Vec<u8>), i32> {
        let Ok(request) = serde_json::from_slice::<Request>(request) else {
            return Err(plumb_plugin::FETCH_BAD_REQUEST);
        };
        let Ok(url) = url::Url::parse(&request.url) else {
            return Err(plumb_plugin::FETCH_BAD_REQUEST);
        };
        if !matches!(url.scheme(), "http" | "https") {
            return Err(plumb_plugin::FETCH_NOT_ALLOWED);
        }
        if !url.host_str().is_some_and(|h| self.manifest.allows(h)) {
            return Err(plumb_plugin::FETCH_NOT_ALLOWED);
        }
        if self.fetches >= MAX_FETCHES {
            return Err(plumb_plugin::FETCH_TOO_MANY);
        }
        let Some(left) = self.deadline.checked_duration_since(Instant::now()) else {
            return Err(plumb_plugin::FETCH_TIME_UP);
        };
        self.fetches += 1;
        let method = match request.method.to_ascii_uppercase().as_str() {
            "GET" => reqwest::Method::GET,
            "POST" => reqwest::Method::POST,
            _ => return Err(plumb_plugin::FETCH_BAD_REQUEST),
        };
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
            Ok((status, body))
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
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PluginItem {
    pub title: String,
    pub url: String,
    /// The site it is on.
    pub site: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<u64>,
}

/// Keeps what a plugin said that a results page can show: `http(s)`
/// links with titles, trimmed, without what safe search leaves out.
fn clean(items: Vec<Item>, safe: SafeSearch) -> Vec<PluginItem> {
    let mut kept = Vec::new();
    for item in items {
        let Ok(url) = url::Url::parse(item.url.trim()) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") || item.url.len() > 2048 {
            continue;
        }
        let title = squash(&item.title, 200);
        if title.is_empty() {
            continue;
        }
        let snippet = item
            .snippet
            .map(|s| squash(&s, 400))
            .filter(|s| !s.is_empty());
        let host = url.host_str().unwrap_or_default().to_string();
        let site = plumb_core::registrable_domain(&host).unwrap_or(host);
        let level = plumb_core::safe::adult_level(
            &site,
            std::iter::once(title.as_str()).chain(snippet.as_deref()),
        );
        if safe.hides(level) {
            continue;
        }
        kept.push(PluginItem {
            title,
            url: url.to_string(),
            site,
            snippet,
            published: item.published,
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

/// A node's plugins. Cheap to clone.
#[derive(Clone, Default)]
pub struct Plugins {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    plugins: Vec<Arc<Plugin>>,
    cache: Mutex<HashMap<CacheKey, (Instant, Vec<PluginItem>)>>,
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

impl Plugins {
    pub fn new(plugins: Vec<Plugin>) -> Self {
        Plugins {
            inner: Arc::new(Inner {
                plugins: plugins.into_iter().map(Arc::new).collect(),
                cache: Mutex::default(),
            }),
        }
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
                        "plugin {} ({}) loaded; it may fetch from {}",
                        plugin.id,
                        plugin.manifest.name,
                        plugin.manifest.hosts.join(", ")
                    );
                    plugins.push(plugin);
                }
                Err(error) => warn!("plugin in {} left out: {error:#}", folder.display()),
            }
        }
        Plugins::new(plugins)
    }

    pub fn is_empty(&self) -> bool {
        self.inner.plugins.is_empty()
    }

    pub fn list(&self) -> impl Iterator<Item = &Plugin> {
        self.inner.plugins.iter().map(Arc::as_ref)
    }

    /// The results of the plugins `query` picks, in the order they were
    /// installed; plugins that fail, are busy or take too long are left
    /// out. Waits at most a few seconds.
    pub async fn search(
        &self,
        query: &str,
        safe: SafeSearch,
        language: Option<&str>,
    ) -> Vec<PluginResults> {
        if self.is_empty() || query.trim().is_empty() {
            return Vec::new();
        }
        let runtime = tokio::runtime::Handle::current();
        let mut runs = tokio::task::JoinSet::new();
        let mut found: Vec<(usize, PluginResults)> = Vec::new();
        for (at, plugin) in self.inner.plugins.iter().enumerate() {
            let Some((keyword, terms)) = plugin.manifest.picks(query) else {
                continue;
            };
            let input = Query {
                text: query.to_string(),
                terms,
                keyword,
                safe: safe.as_str().to_string(),
                language: language.map(str::to_string),
                config: plugin.config.clone(),
            };
            let key = (
                plugin.id.clone(),
                serde_json::to_string(&input).unwrap_or_default(),
            );
            if let Some(results) = self.cached(&key) {
                found.push((at, plugin.results(results)));
                continue;
            }
            let Ok(permit) = Arc::clone(&plugin.running).try_acquire_owned() else {
                debug!("plugin {} is busy; left out", plugin.id);
                continue;
            };
            let plugin = Arc::clone(plugin);
            let runtime = runtime.clone();
            runs.spawn_blocking(move || {
                let _permit = permit;
                let ran = plugin.run(&input, &runtime);
                (at, plugin, key, ran)
            });
        }
        let waited = tokio::time::timeout(SEARCH_TIME + Duration::from_millis(500), async {
            while let Some(done) = runs.join_next().await {
                let Ok((at, plugin, key, ran)) = done else {
                    continue;
                };
                match ran {
                    Ok(items) => {
                        let items = clean(items, safe);
                        self.keep(key, items.clone());
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
            runs.detach_all();
        }
        found.sort_by_key(|(at, _)| *at);
        found
            .into_iter()
            .map(|(_, results)| results)
            .filter(|r| !r.results.is_empty())
            .collect()
    }

    fn cached(&self, key: &CacheKey) -> Option<Vec<PluginItem>> {
        let cache = self.inner.cache.lock().ok()?;
        let (at, items) = cache.get(key)?;
        (at.elapsed() < CACHE_TIME).then(|| items.clone())
    }

    fn keep(&self, key: CacheKey, items: Vec<PluginItem>) {
        let Ok(mut cache) = self.inner.cache.lock() else {
            return;
        };
        if cache.len() >= CACHE_ENTRIES {
            cache.retain(|_, (at, _)| at.elapsed() < CACHE_TIME);
        }
        if cache.len() >= CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(key, (Instant::now(), items));
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
        let (keyword, terms) = self
            .manifest
            .picks(query)
            .unwrap_or_else(|| (None, query.to_string()));
        let input = Query {
            text: query.to_string(),
            terms,
            keyword,
            safe: SafeSearch::Moderate.as_str().to_string(),
            language: None,
            config: self.config.clone(),
        };
        let runtime = tokio::runtime::Handle::current();
        let items = tokio::task::spawn_blocking(move || self.run(&input, &runtime))
            .await
            .context("the plugin's run failed")??;
        Ok(clean(items, SafeSearch::Moderate))
    }
}

/// `plumb try-plugin`: runs the plugin in folder `dir` on `query` and
/// prints its results as JSON.
pub fn try_plugin(dir: &Path, query: &str) -> Result<()> {
    let plugin = Arc::new(Plugin::load(dir)?);
    let runtime = tokio::runtime::Runtime::new().context("starting the runtime")?;
    let items = runtime.block_on(plugin.try_query(query))?;
    println!("{}", serde_json::to_string_pretty(&items)?);
    Ok(())
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
            (func (export "plumb_search") (call $output (i32.const 0) (i32.const {len}))))"#,
        len = output.len()
    )
}

/// Plugins of one plugin, named `name`, that `keyword` runs and that
/// hands back `output`.
#[cfg(test)]
pub(crate) fn answering_plugins(name: &str, keyword: &str, output: &str) -> Plugins {
    let manifest = Manifest {
        name: name.into(),
        about: String::new(),
        hosts: vec!["a.example".into()],
        keywords: vec![keyword.into()],
        always: false,
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
