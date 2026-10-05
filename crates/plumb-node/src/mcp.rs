//! A Model Context Protocol (MCP) server, so AI assistants can ask Plumb
//! "what is the real site for X?" before they open a page, fill in a login
//! or cite a source.
//!
//! It answers JSON-RPC 2.0 messages ([`Mcp::handle`]) with these
//! read-only tools:
//!
//! - `official_site(name)`: the official site for a name, with how sure
//!   Plumb is and why (Wikidata lists it, the name is the site's own, it is
//!   well known);
//! - `check_lookalike(url)`: whether an address is the real site or one
//!   made to look like another (paypal-login.us, twiter.com), and which
//!   site it imitates;
//! - `search(query, limit)`: the normal results, sites and pages, with
//!   what the results page shows above and beside them: an instant answer
//!   (sums, conversions, the time somewhere), the info box, an official
//!   profile and recent headlines;
//! - `site_info(domain)`: what Plumb knows about one site;
//! - `read_page(url)`: a page as plain text, fetched by this node when an
//!   AI app picks it from the results. Only offered to AI apps on the
//!   node's own computer, or to every client with `--mcp-read-pages`: on a
//!   public node it would make an open proxy. Nothing read is kept.
//!
//! Each tool's answer comes twice: as short plain text, one line per
//! result, for the model to read (small local models have little room),
//! and as JSON in `structuredContent` for programs.
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
use plumb_answer::Rates;
use plumb_core::{domain_label, host_of, registrable_domain, search_template_for, truncate_chars};
use plumb_crawl::{PageReader, ReadConfig};
use plumb_index::pages::{place_operator_pages, place_pages};
use plumb_index::{
    without_intent_words, Hit, SearchOptions, SearchResults, Searcher, WELL_KNOWN_LINK_SCORE,
};
use serde_json::{json, Map, Value};

use crate::cli::McpArgs;
use crate::findings::{Finding, Findings};
use crate::rank_config;
use crate::web::answers;
use crate::web::{IndexBackend, SearchBackend, StatusSource, MAX_QUERY_CHARS};

mod text;

pub(crate) use text::answer_line;

/// Protocol versions this server speaks, newest first. A client asking for
/// one of them gets it; any other gets the newest.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// Results `search` returns when the caller does not say...
const DEFAULT_SEARCH_LIMIT: usize = 5;
/// ...and when the search already has a direct answer: an answer found
/// before, a package's card, an instant answer or a site or page named by
/// the query. The rest are mostly sites with one of the query's words, and
/// cost a model tokens for nothing.
const DIRECT_SEARCH_LIMIT: usize = 3;
/// Most results `search` returns.
const MAX_SEARCH_LIMIT: usize = 25;
/// Other candidates `official_site` lists.
const ALTERNATIVES: usize = 3;
/// Most searches one `check_lookalike` runs.
const MAX_LOOKALIKE_SEARCHES: usize = 6;
/// Longest description returned, in characters.
const MAX_DESCRIPTION_CHARS: usize = 300;
/// Characters of a page `read_page` returns when the caller does not say...
const DEFAULT_READ_CHARS: usize = 6_000;
/// ...and the most it returns at once.
const MAX_READ_CHARS: usize = 30_000;
/// Links `read_page` lists when asked for them.
const MAX_LINKS_RETURNED: usize = 60;
/// Headlines `search` returns.
const MAX_HEADLINES: usize = 5;
/// Most packages the `package` tool lists.
const MAX_PACKAGES: usize = 5;
/// Results asked for when looking up whose profile a query asks for.
const PROFILE_SEARCH_LIMIT: usize = 5;
/// Most words of a search whose package `search` guesses ("lodash
/// debounce", "numpy release notes")...
const MAX_GUESS_WORDS: usize = 4;
/// ...and the least popularity of the guessed package: only the best known
/// of its registry, so "express delivery" is not about a package.
const GUESSED_PACKAGE: f32 = 0.75;

/// What the server tells a client about itself when it connects.
const INSTRUCTIONS: &str = "Plumb Search finds websites by name. Before opening a site you are \
     not sure of, call official_site with the name of the company, project or service to get \
     its real address. Before entering credentials or trusting a link, call check_lookalike \
     with the address: it says whether it is the real site or one built to look like another. \
     search returns ordinary results (sites, plus Wikipedia articles, Stack Overflow \
     questions, books and other pages), and with them a direct answer when it can work one \
     out (sums, unit and currency conversions, the time somewhere), facts about what the \
     query names, and recent headlines. site_info describes one site. Plumb knows homepages \
     and names, not the full text of pages, so search by name or topic rather than by \
     question.";

/// Added to [`INSTRUCTIONS`] when `report_finding` is offered.
const FINDINGS_INSTRUCTIONS: &str = " Whenever a search led you to an answer, call \
     report_finding with what you searched for, the page that answered it, why it helped and \
     the answer itself: the next search for it on this computer starts with that answer, so it \
     need not be worked out again. search lists such answers first, as found_before.";

/// Findings listed with a search's results.
const MAX_FOUND_BEFORE: usize = 3;

/// Added to [`INSTRUCTIONS`] when `read_page` is offered.
const READ_INSTRUCTIONS: &str = " To learn what a page says, call read_page with its URL: \
     search to find the right site or page, then read it.";

/// The tools' JSON-RPC server over a [`SearchBackend`]. Blocking: run it
/// off async threads.
pub struct Mcp {
    backend: Arc<dyn SearchBackend>,
    /// The home country searches use unless a call names one.
    country: Option<String>,
    /// Fetches pages for `read_page`; without it the tool is not offered.
    reader: Option<Reader>,
    /// Currency rates for instant answers, when the caller has them.
    rates: Option<Rates>,
    /// The node, for recent headlines; `None` answers without them.
    node: Option<Arc<dyn StatusSource>>,
    /// What the node's plugins found for the `search` call's query.
    plugins: Vec<crate::plugins::PluginResults>,
    /// What agents found before; without them `report_finding` is not
    /// offered.
    findings: Option<Arc<Findings>>,
}

/// What `read_page` fetches pages with: a reader, and the runtime its
/// requests run on (the tools themselves run on a blocking thread).
#[derive(Clone)]
pub struct Reader {
    pages: PageReader,
    runtime: tokio::runtime::Handle,
}

impl Reader {
    pub fn new(pages: PageReader, runtime: tokio::runtime::Handle) -> Self {
        Reader { pages, runtime }
    }

    /// A reader with the usual settings, running on `runtime`.
    pub fn standard(runtime: tokio::runtime::Handle) -> Result<Self> {
        let pages = PageReader::new(ReadConfig::default()).context("making the page reader")?;
        Ok(Reader::new(pages, runtime))
    }
}

/// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

impl Mcp {
    pub fn new(backend: Arc<dyn SearchBackend>, country: Option<String>) -> Self {
        Mcp {
            backend,
            country,
            reader: None,
            rates: None,
            node: None,
            plugins: Vec::new(),
            findings: None,
        }
    }

    /// Offers `report_finding`, keeping findings in `findings`, and lists
    /// those that match a search with its results.
    pub fn with_findings(mut self, findings: Option<Arc<Findings>>) -> Self {
        self.findings = findings;
        self
    }

    /// Offers `read_page`, fetching pages with `reader`.
    pub fn with_reader(mut self, reader: Option<Reader>) -> Self {
        self.reader = reader;
        self
    }

    /// Answers currency conversions with `rates`.
    pub fn with_rates(mut self, rates: Option<Rates>) -> Self {
        self.rates = rates;
        self
    }

    /// Lists the node's recent headlines with search results.
    pub fn with_node(mut self, node: Option<Arc<dyn StatusSource>>) -> Self {
        self.node = node;
        self
    }

    /// Lists what the node's plugins found with `search`'s results; the
    /// caller runs them for [`Mcp::search_query`].
    pub fn with_plugin_results(mut self, plugins: Vec<crate::plugins::PluginResults>) -> Self {
        self.plugins = plugins;
        self
    }

    /// The query of a `search` call, for a caller that fetches currency
    /// rates before handling it.
    pub fn search_query(message: &Value) -> Option<&str> {
        let params = message.get("params")?;
        if message.get("method")?.as_str()? != "tools/call"
            || params.get("name")?.as_str()? != "search"
        {
            return None;
        }
        params.get("arguments")?.get("query")?.as_str()
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
            "initialize" => Ok(initialize(
                &params,
                self.reader.is_some(),
                self.findings.is_some(),
            )),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({
                "tools": tools(self.reader.is_some(), self.findings.is_some())
            })),
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
                    None | Some(Value::Null) => None,
                    Some(limit) => Some(
                        limit
                            .as_u64()
                            .filter(|&n| n > 0)
                            .ok_or((
                                INVALID_PARAMS,
                                "limit must be a positive whole number".into(),
                            ))?
                            .min(MAX_SEARCH_LIMIT as u64) as usize,
                    ),
                };
                let options = self.options(args)?;
                self.search(&query, limit, &options)
            }
            "site_info" => {
                let domain = text_arg(args, "domain")?;
                let options = self.options(args)?;
                self.site_info(&domain, &options)
            }
            "package" => {
                let name = text_arg(args, "name")?;
                let registry = args
                    .get("registry")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|r| !r.is_empty());
                let registry = match registry {
                    None => None,
                    Some(key) => {
                        Some(plumb_core::packages::registry(&key.to_lowercase()).ok_or((
                            INVALID_PARAMS,
                            format!(
                            "registry must be one of {}; got {key:?}",
                            plumb_core::packages::REGISTRIES
                                .iter()
                                .map(|r| r.key)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        ))?)
                    }
                };
                let options = self.options(args)?;
                self.package(&name, registry, &options)
            }
            "report_finding" if self.findings.is_some() => {
                let query = text_arg(args, "query")?;
                let url = text_arg(args, "url")?;
                let why = text_arg(args, "why")?;
                let answer = args
                    .get("answer")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .ok_or((INVALID_PARAMS, "answer is required".to_string()))?
                    .to_string();
                let task = args.get("task").and_then(Value::as_str);
                let options = self.options(args)?;
                self.report_finding(&query, &url, &why, &answer, task, &options)
            }
            "read_page" if self.reader.is_some() => {
                let read = ReadArgs::of(args)?;
                let options = self.options(args)?;
                self.read_page(&read, &options)
            }
            _ => return Err((INVALID_PARAMS, format!("unknown tool {name:?}"))),
        };
        Ok(tool_result(name, answer))
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
        let did_you_mean = results.spelling.as_ref().map(|s| s.query.as_str());
        let Some(top) = results.hits.first() else {
            let mut why = vec!["Plumb knows no site by this name.".to_string()];
            let package_home = self.package_home(name, options);
            if let Some((home, registry)) = &package_home {
                why.push(format!(
                    "The {registry} package of this name gives {home} as its home page."
                ));
            } else if let Some(fixed) = did_you_mean {
                why.push(format!("Did you mean {fixed:?}? Look that up instead."));
            }
            return Ok(json!({
                "name": name,
                "found": false,
                "why": why,
                "did_you_mean": did_you_mean,
                "package_home": package_home.map(|(home, _)| home),
            }));
        };
        let mut why = Vec::new();
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
        let mut confidence = if top.named && (top.official || well_known) && lead >= 0.1 {
            "high"
        } else if top.named || top.official || (well_known && lead >= 0.2) {
            "medium"
        } else {
            "low"
        };
        let mut url = top.url.clone();
        let mut domain = top.domain.clone();
        let mut title = top.title.clone();
        let mut description = top.description.as_deref().map(short);
        let mut alternatives: Vec<Value> = results.hits[1..].iter().map(brief).collect();
        let mut did_you_mean = did_you_mean;
        let mut package_home = None;
        if confidence != "high" {
            // A software package of the name says where its home is:
            // FastAPI's PyPI card names fastapi.tiangolo.com.
            if let Some((home, registry)) = self.package_home(name, options) {
                if registrable_domain(&home).as_deref() == Some(top.domain.as_str()) {
                    why.push(format!(
                        "The {registry} package of this name gives it as its home page."
                    ));
                    url = home;
                    confidence = "high";
                    did_you_mean = None;
                } else if let (Some(home_domain), "low") = (registrable_domain(&home), confidence) {
                    // The best match of the words is a guess; the
                    // package's own home page is not: "FastAPI" is not
                    // xapo.com.
                    why.push(format!(
                        "No site is called exactly this, but the {registry} package of this \
                         name gives it as its home page."
                    ));
                    alternatives.insert(0, brief(top));
                    alternatives.truncate(ALTERNATIVES);
                    url = home;
                    domain = home_domain;
                    title = None;
                    description = None;
                    confidence = "medium";
                    did_you_mean = None;
                } else {
                    why.push(format!(
                        "The {registry} package of this name gives {home} as its home page."
                    ));
                    package_home = Some(home);
                }
            }
        }
        if confidence == "low" {
            why.push(
                "No site is called exactly this; it is the best match of the words.".to_string(),
            );
        }
        Ok(json!({
            "name": name,
            "found": true,
            "domain": domain,
            "url": url,
            "package_home": package_home,
            "title": title,
            "description": description,
            "confidence": confidence,
            "why": why,
            "alternatives": alternatives,
            "did_you_mean": did_you_mean,
        }))
    }

    /// The home page (else the docs) of the most used package called
    /// `name`, and its registry's name.
    fn package_home(&self, name: &str, options: &SearchOptions) -> Option<(String, String)> {
        let found = self.package(name, None, options).ok()?;
        let card = found["packages"].as_array()?.iter().find(|card| {
            card["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(name.trim()))
        })?;
        let home = card["homepage"]
            .as_str()
            .or_else(|| card["docs"].as_str())?
            .to_string();
        Some((home, card["registry"].as_str()?.to_string()))
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
        let mut queries: std::collections::VecDeque<String> =
            name_queries(&host, &domain).into_iter().collect();
        while let Some(query) = queries.pop_front() {
            if searches >= MAX_LOOKALIKE_SEARCHES {
                break;
            }
            searches += 1;
            let results = self.lookup(&query, 1, options)?;
            // A typo of a name ("twiter") is searched as typed; the name
            // it is a typo of is looked up next.
            if let Some(spelling) = results.spelling {
                queries.push_front(spelling.query);
            }
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

    /// `search`: the results a person would see, sites and pages, and
    /// what the results page shows with them. Without a `limit`, at most
    /// [`DEFAULT_SEARCH_LIMIT`] results, or [`DIRECT_SEARCH_LIMIT`] when
    /// the search has a direct answer. A search for a well-known package's
    /// docs or one of its functions gets the package's card too.
    pub fn search(
        &self,
        query: &str,
        limit: Option<usize>,
        options: &SearchOptions,
    ) -> Result<Value> {
        let mut results = self.lookup(query, limit.unwrap_or(DEFAULT_SEARCH_LIMIT), options)?;
        let guessed = if results.pages.iter().any(|p| p.hit.page.package.is_some()) {
            None
        } else {
            self.guess_package(query, options)
        };
        if guessed.is_some() {
            // "fastapi docs" is not "fastai docs".
            results.spelling = None;
        }
        // Placed as the results page places them, which the info box needs.
        let found_pages = results.pages.iter().map(|p| p.hit.clone()).collect();
        let operators = plumb_core::Operators::parse(query);
        let placed = if operators.any() {
            place_operator_pages(&operators, &results.hits, found_pages)
        } else {
            place_pages(query, &results.hits, found_pages)
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let answer = plumb_answer::answer(
            query,
            i64::try_from(now).unwrap_or(i64::MAX),
            self.rates.as_ref(),
        );
        let names_a_page = placed.iter().any(|placed| placed.hit.named);
        let profile = match plumb_core::profiles::services_asked(query) {
            Some((_, name)) if !names_a_page => self
                .lookup(&name, PROFILE_SEARCH_LIMIT, options)
                .ok()
                .and_then(|found| answers::profile_answer(query, &found.pages)),
            _ => None,
        };
        let info = match &profile {
            Some(profile) => answers::info_from_page(&profile.page, &results.hits),
            None if operators.any() => None,
            None => answers::info_box(&results.hits, &placed),
        };
        let recent = self.node.as_ref().and_then(|node| {
            let top = results
                .hits
                .first()
                .map(|hit| (hit.domain.as_str(), hit.named));
            node.recent(query, top)
        });
        let headlines: Vec<Value> = recent
            .iter()
            .flat_map(|recent| recent.headlines.iter().take(MAX_HEADLINES))
            .map(|headline| {
                json!({
                    "title": headline.title,
                    "url": headline.url,
                    "site": headline.domain,
                    "published": crate::web::time_ago(headline.at, now),
                })
            })
            .collect();
        let mut pages: Vec<Value> = guessed
            .iter()
            .map(|page| {
                json!({
                    "title": page.title,
                    "url": page.url,
                    "description": page.description.as_deref().map(short),
                    "set": page.set,
                    "about_site": Value::Null,
                    "position": 1,
                    "package": package_card(page),
                })
            })
            .collect();
        pages.extend(placed.iter().map(|placed| {
            let mut page = json!({
                "title": placed.hit.page.title,
                "url": placed.hit.page.url,
                "description": placed.hit.page.description.as_deref().map(short),
                "set": placed.hit.page.set,
                "about_site": placed.under,
                "position": placed.at + 1,
            });
            if placed.hit.page.package.is_some() {
                page["package"] = package_card(&placed.hit.page);
            }
            page
        }));
        let mut sites: Vec<Value> = results.hits.iter().map(brief).collect();
        let found_before: Vec<Value> = self
            .findings
            .iter()
            .flat_map(|findings| findings.for_query(query, MAX_FOUND_BEFORE))
            .map(|finding| {
                json!({
                    "query": finding.query,
                    "url": finding.url,
                    "why": finding.why,
                    "answer": finding.answer,
                    "task": finding.task,
                    "reported": crate::web::time_ago(finding.at, now),
                })
            })
            .collect();
        let direct = !found_before.is_empty()
            || answer.is_some()
            || pages.iter().any(|page| page.get("package").is_some())
            || names_a_page
            || results.hits.iter().any(|hit| hit.named);
        let cap = match limit {
            Some(limit) => limit,
            None if direct => DIRECT_SEARCH_LIMIT,
            None => DEFAULT_SEARCH_LIMIT,
        };
        cap_results(&mut sites, &mut pages, cap);
        let mut answer_json = json!({
            "query": query,
            "results": sites,
            "pages": pages,
            "site_search": results.site_search,
            "spelling": results.spelling,
        });
        let fields = answer_json.as_object_mut().expect("an object");
        if let Some(answer) = answer {
            fields.insert("answer".into(), json!(answer));
        }
        if let Some(profile) = profile {
            fields.insert("profile".into(), json!(profile));
        }
        if let Some(info) = info {
            fields.insert("about".into(), json!(info));
        }
        // Headlines with a package's name are rarely about the package:
        // "react latest" is not footballers' kids reacting to a new kit.
        if !headlines.is_empty() && !pages.iter().any(|page| page.get("package").is_some()) {
            fields.insert("recent".into(), json!(headlines));
        }
        let from_plugins: Vec<Value> = self
            .plugins
            .iter()
            .map(|found| {
                let results: Vec<Value> = found
                    .results
                    .iter()
                    .map(|item| {
                        json!({
                            "title": item.title,
                            "url": item.url,
                            "site": item.site,
                            "snippet": item.snippet,
                            "published": item.published.map(|at| crate::web::time_ago(at, now)),
                        })
                    })
                    .collect();
                json!({ "plugin": found.name, "results": results })
            })
            .collect();
        if !from_plugins.is_empty() {
            fields.insert("plugins".into(), json!(from_plugins));
        }
        if !found_before.is_empty() {
            fields.insert("found_before".into(), json!(found_before));
        }
        Ok(answer_json)
    }

    /// The well-known package a search is about when it says no registry:
    /// its first word other than filler, of a search of a few words
    /// ("lodash debounce", "numpy release notes", "fastapi docs").
    fn guess_package(
        &self,
        query: &str,
        options: &SearchOptions,
    ) -> Option<plumb_index::pages::Page> {
        if plumb_core::Operators::parse(query).any() {
            return None;
        }
        let words: Vec<&str> = query.split_whitespace().collect();
        if !(2..=MAX_GUESS_WORDS).contains(&words.len()) {
            return None;
        }
        let name = words.into_iter().find(|word| {
            !plumb_core::packages::FILLER_WORDS.contains(&word.to_lowercase().as_str())
        })?;
        let found = self.lookup(&format!("{name} package"), 1, options).ok()?;
        found
            .pages
            .into_iter()
            .map(|placed| placed.hit)
            .find(|hit| hit.page.package.is_some() && hit.popularity >= GUESSED_PACKAGE)
            .map(|hit| hit.page)
    }

    /// `package`: the cards of the packages called `name`, of `registry`
    /// or of any, the most used first.
    pub fn package(
        &self,
        name: &str,
        registry: Option<&plumb_core::packages::Registry>,
        options: &SearchOptions,
    ) -> Result<Value> {
        // "package" asks for any package of the name, the registry's
        // word or language for its own.
        let mut query = format!("{name} package");
        if let Some(word) = registry.and_then(|r| r.words.first().or(r.languages.first())) {
            query.push(' ');
            query.push_str(word);
        }
        let results = self.lookup(&query, 1, options)?;
        let packages: Vec<Value> = results
            .pages
            .iter()
            .map(|placed| &placed.hit.page)
            .filter(|page| page.package.is_some())
            .take(MAX_PACKAGES)
            .map(package_card)
            .collect();
        Ok(json!({
            "name": name,
            "found": !packages.is_empty(),
            "packages": packages,
        }))
    }

    /// `report_finding`: keeps what an agent found, unless its page is a
    /// look-alike of another site.
    pub fn report_finding(
        &self,
        query: &str,
        url: &str,
        why: &str,
        answer: &str,
        task: Option<&str>,
        options: &SearchOptions,
    ) -> Result<Value> {
        let Some(findings) = &self.findings else {
            bail!("this node keeps no findings");
        };
        let finding = Finding::new(query, url, why, answer, task, plumb_core::now_unix())?;
        let check = self.check_lookalike(&finding.url, options)?;
        if check["verdict"] == "lookalike" {
            bail!(
                "{} looks like a look-alike of another site, so it was not kept",
                finding.url
            );
        }
        findings.add(finding.clone())?;
        Ok(json!({
            "kept": true,
            "query": finding.query,
            "url": finding.url,
            "findings": findings.len(),
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

    /// `read_page`: the page's text, and whether its address (after
    /// redirects) is a look-alike.
    pub fn read_page(&self, args: &ReadArgs, options: &SearchOptions) -> Result<Value> {
        let Some(reader) = &self.reader else {
            bail!("this node does not read pages");
        };
        let mut answer = reader.read(args)?;
        if let Some(url) = answer["url"].as_str() {
            if let Ok(check) = self.check_lookalike(url, options) {
                add_site_check(&mut answer, &check);
            }
        }
        Ok(answer)
    }
}

/// `read_page`'s arguments.
#[derive(Debug, Clone)]
pub struct ReadArgs {
    url: String,
    start: usize,
    max_chars: usize,
    links: bool,
    /// Words to jump to: the part returned starts at their first
    /// appearance from `start`.
    find: Option<String>,
}

impl ReadArgs {
    fn of(args: &Map<String, Value>) -> Result<Self, (i64, String)> {
        Ok(ReadArgs {
            url: text_arg(args, "url")?,
            start: whole_number(args, "start")?.unwrap_or(0),
            max_chars: whole_number(args, "max_chars")?
                .unwrap_or(DEFAULT_READ_CHARS)
                .clamp(200, MAX_READ_CHARS),
            links: args.get("links").and_then(Value::as_bool).unwrap_or(false),
            find: args
                .get("find")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|find| !find.is_empty())
                .map(|find| truncate_chars(find, MAX_QUERY_CHARS)),
        })
    }
}

/// Adds `check_lookalike`'s verdict to a `read_page` answer.
fn add_site_check(answer: &mut Value, check: &Value) {
    if let Some(fields) = answer.as_object_mut() {
        fields.insert(
            "site".into(),
            json!({ "verdict": check["verdict"], "imitates": check["imitates"]["domain"] }),
        );
    }
}

/// A tool's answer as `tools/call` returns it: short text for the model and
/// the JSON for programs. A failed search is a tool error the model can read.
fn tool_result(name: &str, answer: Result<Value>) -> Value {
    match answer {
        Ok(answer) => json!({
            "content": [{ "type": "text", "text": text::render(name, &answer) }],
            "structuredContent": answer,
            "isError": false,
        }),
        Err(err) => json!({
            "content": [{ "type": "text", "text": format!("Plumb could not answer: {err:#}") }],
            "isError": true,
        }),
    }
}

impl Reader {
    /// Fetches the page and cuts out the part asked for.
    fn read(&self, args: &ReadArgs) -> Result<Value> {
        let page = self
            .runtime
            .block_on(self.pages.read(&args.url))
            .map_err(anyhow::Error::from)?;
        let domain = registrable_domain(&page.url).or_else(|| {
            url::Url::parse(&page.url)
                .ok()?
                .host_str()
                .map(str::to_string)
        });
        if let Some(domain) = domain {
            let opening: String = page.text.chars().take(2_000).collect();
            if plumb_core::is_bot_check_page(
                &domain,
                page.title.as_deref(),
                None,
                &[],
                Some(&opening),
            ) {
                bail!(
                    "{domain} showed a bot check (a CAPTCHA or \"checking your browser\" page) \
                     instead of the page; try another source"
                );
            }
        }
        let (max_chars, links) = (args.max_chars, args.links);
        let chars: Vec<char> = page.text.chars().collect();
        let total = chars.len();
        let mut start = args.start.min(total);
        let found = args
            .find
            .as_deref()
            .map(|find| match find_from(&chars, find, start) {
                Some(at) => {
                    // From the start of its line, when that is near enough
                    // for the match to stay well inside the text returned.
                    let line = chars[..at]
                        .iter()
                        .rposition(|&c| c == '\n')
                        .map_or(0, |n| n + 1);
                    let near = (max_chars / 3).min(300);
                    start = if at - line <= near { line } else { at };
                    true
                }
                None => false,
            });
        let mut end = (start + max_chars).min(total);
        let mut text: String = chars[start..end].iter().collect();
        if end < total {
            // End at a line break when one is in the second half.
            if let Some(cut) = text.rfind('\n').filter(|&cut| cut > text.len() / 2) {
                text.truncate(cut);
                end = start + text.chars().count();
            }
        }
        let mut answer = json!({
            "url": page.url,
            "title": page.title,
            "text": text.trim_end(),
            "start": start,
            "end": end,
            "length": total,
            "more": end < total,
            "truncated": page.cut,
        });
        let fields = answer.as_object_mut().expect("an object");
        if end < total {
            fields.insert("next_start".into(), json!(end));
        }
        if let Some(found) = found {
            fields.insert("found".into(), json!(found));
        }
        if links {
            let links: Vec<Value> = page
                .links
                .iter()
                .take(MAX_LINKS_RETURNED)
                .map(|(text, url)| json!({ "text": truncate_chars(text, 100), "url": url }))
                .collect();
            fields.insert("links".into(), json!(links));
        }
        Ok(answer)
    }
}

/// Where `find` first appears in `chars` at or after `from`, ignoring case.
fn find_from(chars: &[char], find: &str, from: usize) -> Option<usize> {
    let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
    let find: Vec<char> = find.trim().chars().map(fold).collect();
    if find.is_empty() || find.len() > chars.len() {
        return None;
    }
    (from..=chars.len() - find.len()).find(|&at| {
        chars[at..at + find.len()]
            .iter()
            .zip(&find)
            .all(|(&c, &f)| fold(c) == f)
    })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A JSON-RPC parse error, for a message that is not JSON.
pub fn parse_error() -> Value {
    error(Value::Null, PARSE_ERROR, "the message is not JSON")
}

fn initialize(params: &Value, read_pages: bool, findings: bool) -> Value {
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
        "instructions": format!(
            "{INSTRUCTIONS}{}{}",
            if read_pages { READ_INSTRUCTIONS } else { "" },
            if findings { FINDINGS_INSTRUCTIONS } else { "" },
        ),
    })
}

/// The tools' descriptions, as `tools/list` returns them; `read_pages`
/// adds `read_page`, `findings` `report_finding`.
pub fn tools(read_pages: bool, findings: bool) -> Value {
    let country = json!({
        "type": "string",
        "description": "Optional home country, a two-letter code such as US or DE: its sites \
             rank a little higher. \"any\" for none.",
    });
    let read_only = json!({ "readOnlyHint": true, "idempotentHint": true, "openWorldHint": false });
    let mut tools = json!([
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
            "description": "Search the web (web_search) with Plumb Search: sites by name, best \
                 first, plus Wikipedia articles, Stack Overflow questions, books and other pages \
                 placed among them, package cards (version, install command, docs) when the query \
                 says npm, crate, pip, python or another registry or language, a direct answer for sums, unit and currency conversions and \
                 the time somewhere, facts about what the query names, and recent headlines. \
                 Plumb indexes homepages and names, not the full text of the web, so search for \
                 names and topics, then read a page with read_page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to search for." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_SEARCH_LIMIT, "description": "How many results to return (default 5, or 3 when the search has a direct answer such as a package card or an answer found before)." },
                    "country": country,
                },
                "required": ["query"],
            },
            "annotations": read_only,
        },
        {
            "name": "package",
            "title": "Package",
            "description": "A software package's card from npm, PyPI, crates.io, Go, RubyGems, \
                 Packagist, NuGet or Maven Central, by name: its latest version and release \
                 date, license, install command, and where its docs and code are. Use it \
                 instead of opening the registry's page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The package's name, as installed (\"serde\", \"@types/node\", \"requests\")." },
                    "registry": {
                        "type": "string",
                        "enum": plumb_core::packages::REGISTRIES.iter().map(|r| r.key).collect::<Vec<_>>(),
                        "description": "Optional: only this registry's package.",
                    },
                },
                "required": ["name"],
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
    ]);
    if read_pages {
        tools
            .as_array_mut()
            .expect("an array")
            .push(read_page_tool());
    }
    if findings {
        tools
            .as_array_mut()
            .expect("an array")
            .push(report_finding_tool());
    }
    tools
}

/// `report_finding`'s description.
fn report_finding_tool() -> Value {
    json!({
        "name": "report_finding",
        "title": "Report what a search found",
        "description": "Whenever a search led you to an answer, report it: what you searched \
             for, the page that answered it, why that page helped, and the answer itself. The \
             next search for the same thing on this computer lists it first (found_before), \
             so no agent has to work it out again. Kept on this node only, never shared.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What you searched for, as you searched it." },
                "url": { "type": "string", "description": "The page that had the answer." },
                "why": { "type": "string", "description": "Why that page helped (\"the changelog lists each release with its date\")." },
                "answer": { "type": "string", "description": "The answer you found, in a few sentences, with any version numbers, commands or code it needs." },
                "task": { "type": "string", "description": "Optional: what you were doing (\"upgrading tokio in a web server\")." },
            },
            "required": ["query", "url", "why", "answer"],
        },
        "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
    })
}

/// `read_page`'s description.
fn read_page_tool() -> Value {
    json!({
        "name": "read_page",
        "title": "Read a page",
        "description": "Fetch a web page (web_fetch) and return its text, with headings and \
             lists marked in Markdown, without menus, ads or scripts. Use it after search or \
             official_site to read what a page says. Long pages come in parts: call again with \
             start set to next_start, or pass find to jump to the words you need. Also says \
             whether the address is a look-alike of a better-known site.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The page's address." },
                "start": { "type": "integer", "minimum": 0, "description": "Character to start at, for the next part of a long page (default 0)." },
                "max_chars": { "type": "integer", "minimum": 200, "maximum": MAX_READ_CHARS, "description": "Most characters to return (default 6000)." },
                "links": { "type": "boolean", "description": "Also list the page's links (default false)." },
                "find": { "type": "string", "description": "Jump to the first place these words appear (from start), like Ctrl-F; says found: false when they do not." },
            },
            "required": ["url"],
        },
        "annotations": { "readOnlyHint": true, "idempotentHint": true, "openWorldHint": true },
    })
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

/// An optional whole-number argument.
fn whole_number(args: &Map<String, Value>, name: &str) -> Result<Option<usize>, (i64, String)> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(|n| Some(usize::try_from(n).unwrap_or(usize::MAX)))
            .ok_or((INVALID_PARAMS, format!("{name} must be a whole number"))),
    }
}

/// A package's card in a tool's answer.
fn package_card(page: &plumb_index::pages::Page) -> Value {
    let Some(package) = &page.package else {
        return Value::Null;
    };
    json!({
        "name": package.name,
        "registry": page.set_name(),
        "description": page.description.as_deref().map(short),
        "version": package.version,
        "released": package.released,
        "license": package.license,
        "install": package.install(),
        "docs": package.docs(),
        "repo": package.repo,
        "homepage": package.homepage,
        "url": page.url,
    })
}

/// Leaves the first `cap` results of a search, counted as its text lists
/// them: the `sites` in order, each after the `pages` placed before it
/// (`position`), and the pages placed after the last site at the end.
/// Pages listed under a site (`about_site`) go with it.
fn cap_results(sites: &mut Vec<Value>, pages: &mut Vec<Value>, cap: usize) {
    let alone = |page: &Value| page.get("about_site").is_none_or(Value::is_null);
    let position = |page: &Value| {
        page.get("position")
            .and_then(Value::as_u64)
            .map_or(usize::MAX, |at| at as usize)
    };
    let mut left = cap;
    let mut kept_sites = 0;
    let mut kept_pages = vec![true; pages.len()];
    for slot in 1..=sites.len() + 1 {
        for (kept, page) in kept_pages.iter_mut().zip(pages.iter()) {
            let here = if slot > sites.len() {
                position(page) >= slot
            } else {
                position(page) == slot
            };
            if alone(page) && here {
                if left == 0 {
                    *kept = false;
                } else {
                    left -= 1;
                }
            }
        }
        if slot <= sites.len() && left > 0 {
            left -= 1;
            kept_sites = slot;
        }
    }
    sites.truncate(kept_sites);
    let domains: Vec<&Value> = sites.iter().filter_map(|site| site.get("domain")).collect();
    let mut kept = kept_pages.into_iter();
    pages.retain(|page| {
        kept.next().unwrap_or(false)
            && (alone(page) || page.get("about_site").is_some_and(|d| domains.contains(&d)))
    });
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
    let runtime = crate::runtime()?;
    // Pages are fetched from this computer, whichever node answers the rest.
    let reader = Reader::standard(runtime.handle().clone())?;
    match &args.index {
        Some(index) => {
            let searcher = Searcher::open(index)
                .with_context(|| format!("opening the index in {}", index.display()))?;
            let backend: Arc<dyn SearchBackend> =
                Arc::new(IndexBackend::new(searcher, rank_config(None)));
            let rates = answers::RatesCache::default();
            serve_lines(stdin, stdout, |message| {
                let rates = Mcp::search_query(message)
                    .and_then(|query| runtime.block_on(rates.for_query(query)));
                let mcp = Mcp::new(Arc::clone(&backend), args.country.clone())
                    .with_reader(Some(reader.clone()))
                    .with_rates(rates);
                Ok(mcp.handle(message))
            })
        }
        None => {
            let endpoint = mcp_endpoint(&args.node)?;
            let client = reqwest::Client::builder()
                .user_agent(concat!("plumb-mcp/", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .context("making the HTTP client")?;
            serve_lines(stdin, stdout, |message| {
                if let Some(answer) = read_here(&reader, message, |check| {
                    runtime.block_on(forward(&client, &endpoint, check))
                }) {
                    return Ok(Some(answer));
                }
                let mut answer = runtime.block_on(forward(&client, &endpoint, message))?;
                if let Some(answer) = &mut answer {
                    offer_read_page(message, answer);
                }
                Ok(answer)
            })
        }
    }
}

/// For `plumb mcp --node`: answers a `read_page` call here, asking the node
/// (through `ask`) only whether the page's site is a look-alike. `None`
/// for any other message.
fn read_here(
    reader: &Reader,
    message: &Value,
    ask: impl FnOnce(&Value) -> Result<Option<Value>>,
) -> Option<Value> {
    let params = message.get("params")?;
    if message.get("method")?.as_str()? != "tools/call"
        || params.get("name")?.as_str()? != "read_page"
    {
        return None;
    }
    let id = message.get("id")?.clone();
    let empty = Map::new();
    let args = params
        .get("arguments")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let read = match ReadArgs::of(args) {
        Ok(read) => read,
        Err((code, why)) => return Some(error(id, code, &why)),
    };
    let answer = reader.read(&read).map(|mut answer| {
        // Only the site goes to the node, which may be a public one: the
        // path and query of a page someone reads can hold anything.
        let site = answer["url"]
            .as_str()
            .and_then(|url| url::Url::parse(url).ok())
            .and_then(|url| Some(format!("{}://{}/", url.scheme(), url.host_str()?)));
        if let Some(url) = site.as_deref() {
            let check = json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "tools/call",
                "params": { "name": "check_lookalike", "arguments": { "url": url } },
            });
            if let Ok(Some(reply)) = ask(&check) {
                let check = &reply["result"]["structuredContent"];
                if check.is_object() {
                    add_site_check(&mut answer, check);
                }
            }
        }
        answer
    });
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": tool_result("read_page", answer) }))
}

/// For `plumb mcp --node`: adds `read_page`, answered here, to what the node
/// says it offers.
fn offer_read_page(message: &Value, answer: &mut Value) {
    let method = message.get("method").and_then(Value::as_str);
    let Some(result) = answer.get_mut("result").and_then(Value::as_object_mut) else {
        return;
    };
    match method {
        Some("tools/list") => {
            if let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) {
                if !tools.iter().any(|tool| tool["name"] == "read_page") {
                    tools.push(read_page_tool());
                }
            }
        }
        Some("initialize") => {
            if let Some(Value::String(instructions)) = result.get_mut("instructions") {
                if !instructions.contains("read_page") {
                    instructions.push_str(READ_INSTRUCTIONS);
                }
            }
        }
        _ => {}
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
