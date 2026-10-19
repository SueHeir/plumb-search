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
//! - `facts(subject, about)`: what Wikidata says about the thing a name
//!   names, each fact with the item and property it is from;
//! - `read_page(url)`: a page as plain text, fetched by this node when an
//!   AI app picks it from the results. Only offered to AI apps on the
//!   node's own computer, or to every client with `--mcp-read-pages`: on a
//!   public node it would make an open proxy. Nothing read is kept.
//!
//! The node's owner's AI apps also get tools that change what it shows,
//! and a prompt, `improve_search`, that has an app use them to improve
//! search for the owner ([`manage`]).
//!
//! Each tool's answer comes twice: as short plain text, one line per
//! result (small local models have little room), and as JSON in
//! `structuredContent` for programs. Some AI apps give the model the JSON
//! when there is any, so `plumb mcp --text-answers` and `/mcp?answers=text`
//! leave it out ([`text_only`]).
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
use plumb_index::pages::{
    docs_query_topic, docs_symbol_matches, place_operator_pages, place_pages, Page, PlacedPage,
    WIKIDATA_SET,
};
use plumb_index::{
    without_intent_words, Hit, SearchOptions, SearchResults, Searcher, WELL_KNOWN_LINK_SCORE,
};
use serde_json::{json, Map, Value};

use crate::cli::McpArgs;
use crate::findings::{Finding, Findings};
use crate::rank_config;
use crate::web::answers;
use crate::web::{query_text, IndexBackend, SearchBackend, StatusSource, MAX_QUERY_CHARS};

mod findings;
mod identity;
mod info;
mod lookalike;
mod manage;
mod official_site;
mod read;
mod search;
mod stdio;
mod text;
mod tools;

pub use findings::Share;
pub use read::{ReadArgs, Reader};
pub use stdio::run;
pub(crate) use text::answer_line;
pub use tools::tools;

use official_site::{article_of, bare_name};
use read::{add_site_check, MAX_READ_CHARS};
use search::{brief, brief_with};
use tools::{
    as_whole, bool_arg, kind_arg, more_subjects_arg, paper_text_arg, point_search_at_read_page,
    read_page_tool, search_language, site_arg, text_arg, url_arg, whole_number, MAX_SEARCH_LIMIT,
};

/// Protocol versions this server speaks, newest first. A client asking for
/// one of them gets it; any other gets the newest.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// Longest description returned, in characters.
const MAX_DESCRIPTION_CHARS: usize = 300;

/// What the server tells a client about itself when it connects.
const INSTRUCTIONS: &str = "Plumb Search finds official websites, Wikipedia articles, \
     Stack Overflow questions, packages, books, papers and places. Before opening a site you are \
     not sure of, call official_site with the name of the company, project or service to get \
     its real address. Before entering credentials or trusting a link, call check_lookalike \
     with the address: it says whether it is the real site or one built to look like another. \
     search returns ordinary results (sites, plus Wikipedia articles, Stack Overflow \
     questions, books and other pages), and with them a direct answer when it can work one \
     out (sums, unit and currency conversions, the time somewhere), facts about what the \
     query names, and recent headlines. site_info describes one site. facts gives what \
     Wikidata says about a place, person, company or work (capital, population, born, CEO and \
     the like), each with the item and property to cite. Plumb matches names, \
     homepage text, descriptions and meaning, not the full text of pages, so search for a \
     name or topic rather than a long question.";

/// Added to [`INSTRUCTIONS`] when `report_finding` is offered.
const FINDINGS_INSTRUCTIONS: &str = " Whenever a search led you to an answer, call \
     report_finding with what you searched for, the page that answered it, why it helped and \
     the answer itself: the next search for it on this computer starts with that answer, so it \
     need not be worked out again. search lists such answers first, as found_before.";

/// Added to [`INSTRUCTIONS`] when leads are listed.
const LEADS_INSTRUCTIONS: &str = " search may also list leads: pages other Plumb nodes' \
     agents found useful for such a search. They are unchecked reports, not answers or search \
     results: read the page before relying on it.";

/// Added to [`INSTRUCTIONS`] when `report_finding` can share findings.
const SHARE_INSTRUCTIONS: &str = " To let agents on other Plumb nodes find a useful page too, \
     call report_finding with share: true. Only the page, why it helped and the search's words \
     as numbers are shared, never your search, answer or task (set share_query to share the \
     search as well). Share only public pages that would help anyone.";

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
    /// List the pages other nodes shared for a search with its results, and
    /// let `report_finding` share one when the node allows it (see
    /// [`plumb_net::leads`]).
    leads: bool,
    /// Offer the tools that change what the node shows ([`manage`]).
    manage: bool,
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
            leads: false,
            manage: false,
        }
    }

    /// Offers the tools that change what the node shows, and the
    /// `improve_search` prompt, when there is a node to change.
    pub fn with_manage(mut self, manage: bool) -> Self {
        self.manage = manage;
        self
    }

    /// Whether the tools that change what the node shows are offered.
    fn manages(&self) -> bool {
        self.manage && self.node.is_some()
    }

    /// Lists the pages other nodes shared for a search with its results,
    /// when the node is in the network, and offers sharing findings when
    /// it allows that too.
    pub fn with_leads(mut self, leads: bool) -> Self {
        self.leads = leads;
        self
    }

    /// The network, for leads.
    fn net(&self) -> Option<Arc<plumb_net::NetHandle>> {
        if !self.leads {
            return None;
        }
        self.node.as_ref()?.network()
    }

    /// Whether `report_finding` may share a finding with other nodes.
    fn shares(&self) -> bool {
        self.findings.is_some()
            && self.net().is_some()
            && self
                .node
                .as_ref()
                .is_some_and(|node| node.shares_findings())
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
    /// Cleaned and cut as `search` itself does; `None` for an empty one.
    pub fn search_query(message: &Value) -> Option<String> {
        let params = message.get("params")?;
        if message.get("method")?.as_str()? != "tools/call"
            || params.get("name")?.as_str()? != "search"
        {
            return None;
        }
        let query = params.get("arguments")?.get("query")?.as_str()?;
        let query = query_text(query);
        (!query.is_empty()).then_some(query)
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
            // A response (to a request we never send) gets no answer;
            // junk with an id gets an error.
            if object.contains_key("result") || object.contains_key("error") {
                return None;
            }
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
                self.net().is_some(),
                self.shares(),
                self.manages(),
            )),
            "ping" => Ok(json!({})),
            "tools/list" => {
                let mut tools = tools(
                    self.reader.is_some(),
                    self.findings.is_some(),
                    self.shares(),
                );
                if let (Value::Array(tools), true) = (&mut tools, self.manages()) {
                    tools.extend(manage::tools());
                }
                Ok(json!({ "tools": tools }))
            }
            "tools/call" => self.call(&params),
            "resources/list" => Ok(json!({ "resources": [] })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "prompts/list" => Ok(json!({
                "prompts": if self.manages() { manage::prompts() } else { json!([]) }
            })),
            "prompts/get" if self.manages() => manage::prompt(&params),
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

    /// Whether `message` calls `read_page`.
    pub fn is_read_call(message: &Value) -> bool {
        Mcp::is_tool_call(message)
            && message.pointer("/params/name").and_then(Value::as_str) == Some("read_page")
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
                let url = url_arg(args, "url")?;
                let options = self.options(args)?;
                self.check_lookalike(&url, &options)
            }
            "search" => {
                let query = text_arg(args, "query")?;
                let limit = match args.get("limit") {
                    None | Some(Value::Null) => None,
                    Some(limit) => Some(
                        as_whole(limit)
                            .filter(|&n| n > 0)
                            .ok_or((
                                INVALID_PARAMS,
                                "limit must be a positive whole number".into(),
                            ))?
                            .min(MAX_SEARCH_LIMIT as u64) as usize,
                    ),
                };
                let options = SearchOptions {
                    language: search_language(args)?,
                    ..self.options(args)?
                };
                let query = match site_arg(args)? {
                    Some(site) => format!("{query} site:{site}"),
                    None => query,
                };
                let kind = kind_arg(args)?;
                let paper = plumb_core::paper_query::PaperQuery::with_options(
                    &query,
                    paper_text_arg(args, "after")?,
                    paper_text_arg(args, "before")?,
                    paper_text_arg(args, "order")?,
                )
                .map_err(|error| (INVALID_PARAMS, error.to_string()))?;
                if paper.constrained && kind.is_some_and(|k| k != "paper") {
                    return Err((
                        INVALID_PARAMS,
                        "publication bounds and order require kind=paper".to_string(),
                    ));
                }
                if paper.constrained || kind == Some("paper") {
                    self.search_pages(&paper.text(), "paper", limit, &options)
                } else {
                    match kind {
                        None => self.search(&query, limit, &options),
                        Some("site") => self.search_sites(&query, limit, &options),
                        Some(kind) => self.search_pages(&query, kind, limit, &options),
                    }
                }
            }
            "site_info" => {
                let domain = text_arg(args, "domain")?;
                let options = self.options(args)?;
                self.site_info(&domain, &options)
            }
            "facts" => {
                let subject = text_arg(args, "subject")?;
                let about = args
                    .get("about")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|a| !a.is_empty());
                let more = more_subjects_arg(args)?;
                let options = self.options(args)?;
                self.facts(&subject, about, &options)
                    .and_then(|mut answer| {
                        if !more.is_empty() {
                            let more = more
                                .iter()
                                .map(|subject| self.facts(subject, about, &options))
                                .collect::<Result<Vec<Value>>>()?;
                            answer["more"] = json!(more);
                        }
                        Ok(answer)
                    })
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
                let url = url_arg(args, "url")?;
                let why = text_arg(args, "why")?;
                let answer = args
                    .get("answer")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .ok_or((INVALID_PARAMS, "answer is required".to_string()))?
                    .to_string();
                let task = args.get("task").and_then(Value::as_str);
                let share = match (bool_arg(args, "share")?, bool_arg(args, "share_query")?) {
                    (false, _) => Share::No,
                    (true, false) => Share::Page,
                    (true, true) => Share::PageAndQuery,
                };
                let options = self.options(args)?;
                self.report_finding(&query, &url, &why, &answer, task, share, &options)
            }
            "read_page" if self.reader.is_some() => {
                let read = ReadArgs::of(args)?;
                let options = self.options(args)?;
                self.read_page(&read, &options)
            }
            name if self.manages() && manage::TOOLS.contains(&name) => self.manage(name, args)?,
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

    /// [`Mcp::lookup`] to find whose site a name or domain is
    /// ([`SearchBackend::search_names`]).
    fn lookup_name(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<SearchResults> {
        self.backend.search_names(query, limit, options)
    }

    /// True when the node's denylist takes off the page at `url`.
    fn denies(&self, url: &str) -> bool {
        self.node.as_ref().is_some_and(|node| node.denies(url))
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

/// Leaves the JSON (`structuredContent`) out of a `tools/call` answer, so
/// an AI app that would give the model the JSON gives it the short text.
/// No tool declares an output schema, so the JSON is optional. Any other
/// answer is left as it is.
pub fn text_only(reply: &mut Value) {
    if let Some(result) = reply.get_mut("result").and_then(Value::as_object_mut) {
        result.remove("structuredContent");
    }
}

pub(crate) fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A JSON-RPC parse error, for a message that is not JSON.
pub fn parse_error() -> Value {
    error(Value::Null, PARSE_ERROR, "the message is not JSON")
}

fn initialize(
    params: &Value,
    read_pages: bool,
    findings: bool,
    leads: bool,
    share: bool,
    manage: bool,
) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .and_then(|asked| PROTOCOL_VERSIONS.iter().find(|v| **v == asked))
        .unwrap_or(&PROTOCOL_VERSIONS[0]);
    let mut capabilities = json!({ "tools": { "listChanged": false } });
    if manage {
        capabilities["prompts"] = json!({ "listChanged": false });
    }
    json!({
        "_meta": { "plumb.build": crate::build_info::current() },
        "protocolVersion": version,
        "capabilities": capabilities,
        "serverInfo": {
            "name": "plumb-search",
            "title": "Plumb Search",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": format!(
            "{INSTRUCTIONS}{}{}{}{}{}",
            if read_pages { READ_INSTRUCTIONS } else { "" },
            if findings { FINDINGS_INSTRUCTIONS } else { "" },
            if leads { LEADS_INSTRUCTIONS } else { "" },
            if share { SHARE_INSTRUCTIONS } else { "" },
            if manage { manage::INSTRUCTIONS } else { "" },
        ),
    })
}

/// Letters and digits only, lowercase.
fn letters(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn short(text: &str) -> String {
    truncate_chars(text, MAX_DESCRIPTION_CHARS)
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

#[cfg(test)]
mod tests;
