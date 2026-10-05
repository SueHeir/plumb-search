//! A Model Context Protocol (MCP) server, so AI assistants can ask Plumb
//! "what is the real site for X?" before they open a page, fill in a login
//! or cite a source.
//!
//! It answers JSON-RPC 2.0 messages ([`Mcp::handle`]) with four read-only
//! tools:
//!
//! - `official_site(name)`: the official site for a name, with how sure
//!   Plumb is and why (Wikidata lists it, the name is the site's own, it is
//!   well known);
//! - `check_lookalike(url)`: whether an address is the real site or one
//!   made to look like another (paypal-login.us, twiter.com), and which
//!   site it imitates;
//! - `search(query, limit)`: the normal results, sites and pages, as JSON;
//! - `site_info(domain)`: what Plumb knows about one site.
//!
//! Two transports carry it:
//!
//! - a node serves `POST /mcp` on its web port ("Streamable HTTP",
//!   answering each message with plain JSON; see [`crate::web`]);
//! - `plumb mcp` speaks it over stdin and stdout for AI apps on this
//!   computer, answering from a local index (`--index`) or passing each
//!   message on to a node's `/mcp` (`--node`, by default plumbsearch.org).

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use plumb_core::{domain_label, host_of, registrable_domain, search_template_for, truncate_chars};
use plumb_index::{
    without_intent_words, Hit, SearchOptions, SearchResults, Searcher, WELL_KNOWN_LINK_SCORE,
};
use serde_json::{json, Map, Value};

use crate::cli::McpArgs;
use crate::rank_config;
use crate::web::{IndexBackend, SearchBackend, MAX_QUERY_CHARS};

/// Protocol versions this server speaks, newest first. A client asking for
/// one of them gets it; any other gets the newest.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// Results `search` returns when the caller does not say.
const DEFAULT_SEARCH_LIMIT: usize = 10;
/// Most results `search` returns.
const MAX_SEARCH_LIMIT: usize = 25;
/// Other candidates `official_site` lists.
const ALTERNATIVES: usize = 3;
/// Most searches one `check_lookalike` runs.
const MAX_LOOKALIKE_SEARCHES: usize = 6;
/// Longest description returned, in characters.
const MAX_DESCRIPTION_CHARS: usize = 300;

/// What the server tells a client about itself when it connects.
const INSTRUCTIONS: &str = "Plumb Search finds websites by name. Before opening a site you are \
     not sure of, call official_site with the name of the company, project or service to get \
     its real address. Before entering credentials or trusting a link, call check_lookalike \
     with the address: it says whether it is the real site or one built to look like another. \
     search returns ordinary results (sites, plus Wikipedia articles and other pages) and \
     site_info describes one site. Plumb knows homepages and names, not the full text of \
     pages, so search by name rather than by question.";

/// The tools' JSON-RPC server over a [`SearchBackend`]. Blocking: run it
/// off async threads.
pub struct Mcp {
    backend: Arc<dyn SearchBackend>,
    /// The home country searches use unless a call names one.
    country: Option<String>,
}

/// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

impl Mcp {
    pub fn new(backend: Arc<dyn SearchBackend>, country: Option<String>) -> Self {
        Mcp { backend, country }
    }

    /// Answers one JSON-RPC message; `None` for a notification or a
    /// response, which get no answer.
    pub fn handle(&self, message: &Value) -> Option<Value> {
        let Some(object) = message.as_object() else {
            return Some(error(
                Value::Null,
                INVALID_REQUEST,
                "expected a JSON object",
            ));
        };
        let id = object.get("id").cloned();
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            // A response to something we never send, or junk.
            return id.map(|id| error(id, INVALID_REQUEST, "expected a method"));
        };
        // A notification (no id) gets no answer, whatever it says.
        let id = id?;
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => self.call(&params),
            "resources/list" => Ok(json!({ "resources": [] })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "prompts/list" => Ok(json!({ "prompts": [] })),
            _ => Err((METHOD_NOT_FOUND, format!("unknown method {method:?}"))),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => error(id, code, &message),
        })
    }

    /// Whether `message` calls a tool, which costs searches; the HTTP
    /// server rate-limits only those.
    pub fn is_tool_call(message: &Value) -> bool {
        message.get("method").and_then(Value::as_str) == Some("tools/call")
    }

    /// `tools/call`: a tool's answer, as text for the model and as
    /// structured content. A failed search is a tool error the model can
    /// read; bad arguments are a protocol error.
    fn call(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, "expected a tool name".to_string()))?;
        let empty = Map::new();
        let args = match params.get("arguments") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(args)) => args,
            Some(_) => return Err((INVALID_PARAMS, "arguments must be an object".to_string())),
        };
        let answer = match name {
            "official_site" => {
                let name = text_arg(args, "name")?;
                let options = self.options(args)?;
                self.official_site(&name, &options)
            }
            "check_lookalike" => {
                let url = text_arg(args, "url")?;
                let options = self.options(args)?;
                self.check_lookalike(&url, &options)
            }
            "search" => {
                let query = text_arg(args, "query")?;
                let limit = match args.get("limit") {
                    None | Some(Value::Null) => DEFAULT_SEARCH_LIMIT,
                    Some(limit) => limit
                        .as_u64()
                        .filter(|&n| n > 0)
                        .ok_or((
                            INVALID_PARAMS,
                            "limit must be a positive whole number".into(),
                        ))?
                        .min(MAX_SEARCH_LIMIT as u64) as usize,
                };
                let options = self.options(args)?;
                self.search(&query, limit, &options)
            }
            "site_info" => {
                let domain = text_arg(args, "domain")?;
                let options = self.options(args)?;
                self.site_info(&domain, &options)
            }
            _ => return Err((INVALID_PARAMS, format!("unknown tool {name:?}"))),
        };
        Ok(match answer {
            Ok(answer) => json!({
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string_pretty(&answer).unwrap_or_default(),
                }],
                "structuredContent": answer,
                "isError": false,
            }),
            Err(err) => json!({
                "content": [{ "type": "text", "text": format!("Plumb could not answer: {err:#}") }],
                "isError": true,
            }),
        })
    }

    /// The search choices of a call: its `country` (a two-letter code, or
    /// `any` for none), else the server's.
    fn options(&self, args: &Map<String, Value>) -> Result<SearchOptions, (i64, String)> {
        let country = match args.get("country").and_then(Value::as_str).map(str::trim) {
            None | Some("") => self.country.clone(),
            Some(code) if code.eq_ignore_ascii_case("any") => None,
            Some(code) => Some(plumb_core::normalize_country(code).ok_or((
                INVALID_PARAMS,
                format!("country must be a two-letter code such as US, or \"any\"; got {code:?}"),
            ))?),
        };
        Ok(SearchOptions {
            country,
            ..SearchOptions::default()
        })
    }

    fn lookup(&self, query: &str, limit: usize, options: &SearchOptions) -> Result<SearchResults> {
        self.backend.search_full(query, limit, options)
    }

    /// `official_site`: the site the name names, best first.
    pub fn official_site(&self, name: &str, options: &SearchOptions) -> Result<Value> {
        let results = self.lookup(name, 1 + ALTERNATIVES, options)?;
        let Some(top) = results.hits.first() else {
            return Ok(json!({
                "name": name,
                "found": false,
                "why": ["Plumb knows no site by this name."],
            }));
        };
        let mut why = Vec::new();
        if let Some(spelling) = results.spelling.as_ref().filter(|s| s.applied) {
            why.push(format!("Read as {:?}, a likely typo.", spelling.query));
        }
        if top.official {
            why.push("Wikidata lists it as an official website.".to_string());
        }
        if top.named {
            why.push("The name is the site's own name or address.".to_string());
        }
        let well_known = top.link_score >= WELL_KNOWN_LINK_SCORE;
        if well_known {
            why.push("It is a well-known site, linked from many others.".to_string());
        }
        let rivals: Vec<&Hit> = results.hits[1..]
            .iter()
            .filter(|hit| hit.named || hit.official)
            .collect();
        if !rivals.is_empty() {
            let names: Vec<&str> = rivals.iter().map(|hit| hit.domain.as_str()).collect();
            why.push(format!(
                "Other sites also go by this name: {}.",
                names.join(", ")
            ));
        }
        // How far ahead of the next site it is, as a share of its score.
        let lead = results.hits.get(1).map_or(1.0, |next| {
            (top.score - next.score) / top.score.max(f32::EPSILON)
        });
        let confidence = if top.named && (top.official || well_known) && lead >= 0.1 {
            "high"
        } else if top.named || top.official || (well_known && lead >= 0.2) {
            "medium"
        } else {
            why.push(
                "No site is called exactly this; it is the best match of the words.".to_string(),
            );
            "low"
        };
        Ok(json!({
            "name": name,
            "found": true,
            "domain": top.domain,
            "url": top.url,
            "title": top.title,
            "description": top.description.as_deref().map(short),
            "confidence": confidence,
            "why": why,
            "alternatives": results.hits[1..].iter().map(brief).collect::<Vec<_>>(),
        }))
    }

    /// `check_lookalike`: whether `input` is a real site or imitates one.
    pub fn check_lookalike(&self, input: &str, options: &SearchOptions) -> Result<Value> {
        let Some(host) = host_of(input) else {
            bail!("{input:?} is not a web address or domain name");
        };
        let Some(domain) = registrable_domain(&host) else {
            bail!("{host:?} has no registrable domain");
        };
        let mut searches = 0;
        // The site itself: a typed hostname names it.
        let own = self.lookup(&domain, 1, options)?;
        searches += 1;
        let site = own.hits.into_iter().find(|hit| hit.domain == domain);
        let site_trusted = site
            .as_ref()
            .is_some_and(|hit| hit.official || hit.link_score >= WELL_KNOWN_LINK_SCORE);

        // What its names spell: the label ("paypal-login" -> "paypal
        // login"), then each word of it and of any subdomain
        // ("paypal.com.secure-check.io" -> "paypal").
        let mut imitated: Option<Hit> = None;
        let squashed_host = squash(&host);
        for query in name_queries(&host, &domain) {
            if searches >= MAX_LOOKALIKE_SEARCHES {
                break;
            }
            searches += 1;
            let results = self.lookup(&query, 1, options)?;
            let Some(top) = results.hits.into_iter().next() else {
                continue;
            };
            if top.domain == domain || !(top.official || top.link_score >= WELL_KNOWN_LINK_SCORE) {
                continue;
            }
            if !resembles(&squashed_host, &squash(&domain_label(&domain)), &top.domain) {
                continue;
            }
            if imitated
                .as_ref()
                .is_none_or(|best| top.link_score > best.link_score)
            {
                imitated = Some(top);
            }
        }

        let mut reasons = Vec::new();
        if host != domain {
            reasons.push(format!("{host} is part of {domain}."));
        }
        let verdict = match (&site, &imitated) {
            (Some(site), _) if site.official => {
                reasons.push("Wikidata lists it as an official website.".to_string());
                "official"
            }
            (Some(_), Some(other)) if site_trusted => {
                reasons.push(format!(
                    "It is a well-known site in its own right, though its name is close to {}.",
                    other.domain
                ));
                "known_site"
            }
            (Some(_), None) if site_trusted => {
                reasons.push("It is a well-known site, linked from many others.".to_string());
                "known_site"
            }
            (_, Some(other)) => {
                reasons.push(format!(
                    "Its address borrows the name of {}, a far better-known site.",
                    other.domain
                ));
                if site.is_none() {
                    reasons.push("Plumb does not list it as a site of its own.".to_string());
                } else {
                    reasons.push("Few other sites link to it.".to_string());
                }
                "lookalike"
            }
            (Some(_), None) => {
                reasons.push(
                    "Plumb knows the site but it is little known; nothing suggests it imitates \
                     another site."
                        .to_string(),
                );
                "little_known"
            }
            (None, None) => {
                reasons.push(
                    "Plumb does not know this site. That alone proves nothing either way."
                        .to_string(),
                );
                "unknown"
            }
        };
        Ok(json!({
            "input": input,
            "host": host,
            "domain": domain,
            "verdict": verdict,
            "lookalike": verdict == "lookalike",
            "reasons": reasons,
            "imitates": (verdict == "lookalike").then(|| imitated.as_ref().map(brief)).flatten(),
            "site": site.as_ref().map(brief),
        }))
    }

    /// `search`: the results a person would see, sites and pages.
    pub fn search(&self, query: &str, limit: usize, options: &SearchOptions) -> Result<Value> {
        let results = self.lookup(query, limit, options)?;
        let pages: Vec<Value> = results
            .pages
            .iter()
            .map(|placed| {
                json!({
                    "title": placed.hit.page.title,
                    "url": placed.hit.page.url,
                    "description": placed.hit.page.description.as_deref().map(short),
                    "set": placed.hit.page.set,
                    "about_site": placed.under,
                    "position": placed.at + 1,
                })
            })
            .collect();
        Ok(json!({
            "query": query,
            "results": results.hits.iter().map(brief).collect::<Vec<_>>(),
            "pages": pages,
            "site_search": results.site_search,
            "spelling": results.spelling,
        }))
    }

    /// `site_info`: one site's entry.
    pub fn site_info(&self, input: &str, options: &SearchOptions) -> Result<Value> {
        let Some(domain) = registrable_domain(input) else {
            bail!("{input:?} is not a web address or domain name");
        };
        let results = self.lookup(&domain, 3, options)?;
        let Some(hit) = results.hits.iter().find(|hit| hit.domain == domain) else {
            return Ok(json!({ "domain": domain, "found": false }));
        };
        let pages: Vec<Value> = results
            .pages
            .iter()
            .filter(|placed| placed.under.as_deref() == Some(domain.as_str()))
            .map(|placed| json!({ "title": placed.hit.page.title, "url": placed.hit.page.url }))
            .collect();
        Ok(json!({
            "domain": domain,
            "found": true,
            "url": hit.url,
            "title": hit.title,
            "description": hit.description.as_deref().map(short),
            "official": hit.official,
            "well_known": hit.link_score >= WELL_KNOWN_LINK_SCORE,
            "popularity": round(hit.link_score),
            "country": hit.country,
            "searchable": search_template_for(&domain).is_some(),
            "pages": pages,
        }))
    }
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A JSON-RPC parse error, for a message that is not JSON.
pub fn parse_error() -> Value {
    error(Value::Null, PARSE_ERROR, "the message is not JSON")
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .and_then(|asked| PROTOCOL_VERSIONS.iter().find(|v| **v == asked))
        .unwrap_or(&PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "plumb-search",
            "title": "Plumb Search",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

/// The tools' descriptions, as `tools/list` returns them.
pub fn tools() -> Value {
    let country = json!({
        "type": "string",
        "description": "Optional home country, a two-letter code such as US or DE: its sites \
             rank a little higher. \"any\" for none.",
    });
    let read_only = json!({ "readOnlyHint": true, "idempotentHint": true, "openWorldHint": false });
    json!([
        {
            "name": "official_site",
            "title": "Official site",
            "description": "The official website for a company, organization, product, project \
                 or service, by name (\"PayPal\", \"rust docs\", \"IRS\"). Returns the domain and \
                 URL, a confidence (high, medium, low), the reasons, and other candidates. Use \
                 it instead of guessing a URL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The name, optionally followed by what you want from the site (\"chase login\")." },
                    "country": country,
                },
                "required": ["name"],
            },
            "annotations": read_only,
        },
        {
            "name": "check_lookalike",
            "title": "Check for a look-alike site",
            "description": "Whether a URL or domain is a real site or one made to look like \
                 another (paypal-login.us, twiter.com). Returns a verdict (official, known_site, \
                 little_known, lookalike or unknown), the reasons, and the real site it imitates. \
                 Use it before entering credentials or trusting a link.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "A URL or a domain name." },
                    "country": country,
                },
                "required": ["url"],
            },
            "annotations": read_only,
        },
        {
            "name": "search",
            "title": "Search",
            "description": "Plumb Search results for a query: sites by name, best first, plus \
                 pages such as Wikipedia articles placed among them. Plumb indexes homepages and \
                 names, not the full text of the web, so search for names and topics, not \
                 questions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to search for." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_SEARCH_LIMIT, "description": "How many sites to return (default 10)." },
                    "country": country,
                },
                "required": ["query"],
            },
            "annotations": read_only,
        },
        {
            "name": "site_info",
            "title": "Site info",
            "description": "What Plumb knows about one site: its title and description, whether \
                 Wikidata lists it as an official website, how well known it is, its country \
                 and pages about it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "domain": { "type": "string", "description": "A domain name or URL." },
                    "country": country,
                },
                "required": ["domain"],
            },
            "annotations": read_only,
        },
    ])
}

/// A required, non-empty text argument, cut to [`MAX_QUERY_CHARS`].
fn text_arg(args: &Map<String, Value>, name: &str) -> Result<String, (i64, String)> {
    let text = args
        .get(name)
        .and_then(Value::as_str)
        .map(|text| truncate_chars(&plumb_core::collapse_whitespace(text), MAX_QUERY_CHARS))
        .unwrap_or_default();
    if text.is_empty() {
        return Err((INVALID_PARAMS, format!("{name} is required")));
    }
    Ok(text)
}

/// A site in a tool's answer.
fn brief(hit: &Hit) -> Value {
    json!({
        "domain": hit.domain,
        "url": hit.url,
        "title": hit.title,
        "description": hit.description.as_deref().map(short),
        "official": hit.official,
        "well_known": hit.link_score >= WELL_KNOWN_LINK_SCORE,
        "country": hit.country,
    })
}

fn short(text: &str) -> String {
    truncate_chars(text, MAX_DESCRIPTION_CHARS)
}

fn round(score: f32) -> f64 {
    (f64::from(score) * 100.0).round() / 100.0
}

/// Words in host names that say nothing about whose site it is.
const FILLER_WORDS: &[&str] = &[
    "www",
    "web",
    "secure",
    "security",
    "login",
    "signin",
    "logon",
    "account",
    "accounts",
    "verify",
    "verification",
    "update",
    "help",
    "support",
    "service",
    "services",
    "online",
    "official",
    "auth",
    "app",
    "apps",
    "mobile",
    "portal",
    "my",
    "the",
    "center",
    "centre",
    "customer",
    "billing",
    "pay",
    "payment",
    "wallet",
    "info",
    "home",
    "site",
    "mail",
    "com",
    "net",
    "org",
    "co",
];

/// What to search for to find the site a host name borrows from: its
/// label as words (without words like "login" at the end), then the other
/// words of the label and the subdomains, longest first.
fn name_queries(host: &str, domain: &str) -> Vec<String> {
    let label = domain_label(domain);
    let label_words = label.replace(['-', '_'], " ");
    let mut queries = vec![without_intent_words(&label_words).unwrap_or(label_words)];
    let subdomains = host.strip_suffix(domain).unwrap_or("");
    let mut words: Vec<String> = subdomains
        .split(['.', '-', '_'])
        .chain(label.split(['-', '_']))
        .map(str::to_ascii_lowercase)
        .filter(|word| word.len() >= 4 && !FILLER_WORDS.contains(&word.as_str()))
        .collect();
    words.sort_by_key(|word| std::cmp::Reverse(word.len()));
    let mut seen: HashSet<String> = queries.iter().cloned().collect();
    for word in words {
        if seen.insert(word.clone()) {
            queries.push(word);
        }
    }
    queries
}

/// Letters and digits only, lowercase, with digits and letter pairs that
/// pass for letters read as them: `paypa1` -> `paypal`, `rnicrosoft` ->
/// `microsoft`.
fn squash(text: &str) -> String {
    let plain: String = text
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| match c.to_ascii_lowercase() {
            '0' => 'o',
            '1' => 'l',
            '3' => 'e',
            '5' => 's',
            c => c,
        })
        .collect();
    plain.replace("rn", "m").replace("vv", "w")
}

/// Whether a host whose letters are `host` and whose registrable label's
/// are `label` borrows the name of the site `real`: it spells the real
/// site's label out (paypal-login.us, paypal.com.example.io) or its label
/// is a typo of it (twiter.com, paypa1.com).
fn resembles(host: &str, label: &str, real: &str) -> bool {
    let real = squash(&domain_label(real));
    if real.len() < 3 {
        return false;
    }
    if host.contains(&real) {
        return true;
    }
    let allowed = if real.len() >= 8 { 2 } else { 1 };
    real.len() >= 4 && edit_distance(label, &real) <= allowed
}

/// Levenshtein distance, counting a swap of neighbours as one edit.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut rows = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    rows[0] = (0..=b.len()).collect();
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (rows[i - 1][j] + 1)
                .min(rows[i][j - 1] + 1)
                .min(rows[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = best;
        }
    }
    rows[a.len()][b.len()]
}

/// `plumb mcp`: the server over stdin and stdout, one JSON message per
/// line, until stdin closes. Logs go to stderr, as always.
pub fn run(args: McpArgs) -> Result<()> {
    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    match &args.index {
        Some(index) => {
            let searcher = Searcher::open(index)
                .with_context(|| format!("opening the index in {}", index.display()))?;
            let backend = IndexBackend::new(searcher, rank_config(None));
            let mcp = Mcp::new(Arc::new(backend), args.country.clone());
            serve_lines(stdin, stdout, |message| Ok(mcp.handle(message)))
        }
        None => {
            let endpoint = mcp_endpoint(&args.node)?;
            let client = reqwest::Client::builder()
                .user_agent(concat!("plumb-mcp/", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .context("making the HTTP client")?;
            let runtime = crate::runtime()?;
            serve_lines(stdin, stdout, |message| {
                runtime.block_on(forward(&client, &endpoint, message))
            })
        }
    }
}

/// The `/mcp` address of a node's web address.
fn mcp_endpoint(node: &str) -> Result<url::Url> {
    let mut url = url::Url::parse(node).with_context(|| format!("reading the address {node:?}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("--node must be an http:// or https:// address, not {node:?}");
    }
    if !url.path().ends_with("/mcp") {
        let path = format!("{}/mcp", url.path().trim_end_matches('/'));
        url.set_path(&path);
    }
    Ok(url)
}

/// Passes one message on to a node's `/mcp` and returns its answer.
async fn forward(
    client: &reqwest::Client,
    endpoint: &url::Url,
    message: &Value,
) -> Result<Option<Value>> {
    let response = client
        .post(endpoint.clone())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(serde_json::to_vec(message).unwrap_or_default())
        .send()
        .await;
    let id = message.get("id").cloned();
    let failed = |why: String| Ok(id.clone().map(|id| error(id, -32000, &why)));
    let response = match response {
        Ok(response) => response,
        Err(err) => return failed(format!("cannot reach {endpoint}: {err}")),
    };
    let status = response.status();
    if status == reqwest::StatusCode::ACCEPTED {
        return Ok(None);
    }
    let body = response.text().await.unwrap_or_default();
    match serde_json::from_str::<Value>(&body) {
        Ok(answer) => Ok(Some(answer)),
        Err(_) if status.is_success() => failed(format!("{endpoint} did not answer with JSON")),
        Err(_) => failed(format!("{endpoint} answered {status}")),
    }
}

/// Reads one JSON-RPC message per line from `input` and writes each answer
/// as one line to `output`.
fn serve_lines(
    input: impl BufRead,
    mut output: impl Write,
    mut answer: impl FnMut(&Value) -> Result<Option<Value>>,
) -> Result<()> {
    for line in input.lines() {
        let line = line.context("reading stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => answer(&message)?,
            Err(_) => Some(parse_error()),
        };
        if let Some(reply) = reply {
            serde_json::to_writer(&mut output, &reply).context("writing stdout")?;
            output.write_all(b"\n").context("writing stdout")?;
            output.flush().context("writing stdout")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
