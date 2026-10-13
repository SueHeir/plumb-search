//! The tools as listed to clients (names, descriptions and argument
//! schemas), and reading the arguments a call brings.

use super::*;

/// Most results `search` returns.
pub(super) const MAX_SEARCH_LIMIT: usize = 25;

/// Most subjects `facts` answers in one call, `subject` and
/// `more_subjects` together.
pub(super) const MAX_FACT_SUBJECTS: usize = 10;

/// The kinds of result `search` can be asked for alone (`kind`), each with
/// what it is: `site` for sites without pages, the others for the pages of
/// one kind ([`kind_keeps`]).
const KINDS: &[(&str, &str)] = &[
    ("site", "sites, without pages"),
    ("article", "Wikipedia articles"),
    (
        "question",
        "Stack Overflow and other Stack Exchange questions",
    ),
    ("package", "packages, with their cards"),
    ("repo", "GitHub repositories"),
    ("book", "books, from Open Library"),
    ("paper", "scholarly papers, from OpenAlex"),
    (
        "docs",
        "pages of software docs (MDN, Python's docs and others)",
    ),
];

/// The language filter of a `search` call: its `language` (a code such as
/// `en`, or `any` for none), else English. Sites that do not say their
/// language stay either way.
pub(super) fn search_language(args: &Map<String, Value>) -> Result<Option<String>, (i64, String)> {
    match args.get("language").and_then(Value::as_str).map(str::trim) {
        None | Some("") => Ok(Some("en".to_string())),
        Some(code) if code.eq_ignore_ascii_case("any") => Ok(None),
        Some(code) => plumb_core::language_code(code).map(Some).ok_or((
            INVALID_PARAMS,
            format!("language must be a code such as en or de, or \"any\"; got {code:?}"),
        )),
    }
}

/// The tools' descriptions, as `tools/list` returns them; `read_pages`
/// adds `read_page`, `findings` `report_finding`, and `share` its choice to
/// share a finding with other nodes.
pub fn tools(read_pages: bool, findings: bool, share: bool) -> Value {
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
                 it instead of guessing a URL. Unresolved identity returns found:false with \
                 suggestions; task words and spelling corrections do not establish ownership. \
                 A jurisdiction named in the query takes precedence over the country preference.",
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
                 little_known, lookalike, suspected or unknown), the reasons from indexed \
                 evidence, and the real site it resembles. Suspected means affiliation \
                 is unverified and impersonation is not established; lookalike is true \
                 for both. \
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
            "description": "Search the web (web_search) with Plumb Search: sites by name or topic, \
                 best first, plus Wikipedia articles, Stack Overflow questions, books and other pages \
                 placed among them, package cards (version, install command, docs) when the query \
                 says npm, crate, pip, python or another registry or language, a direct answer for sums, unit and currency conversions and \
                 the time somewhere, facts about what the query names, recent headlines, and local places \
                 when the query includes a town (restaurants in Denver). Near me requires an actual location; \
                 country alone is not a location. Package cards supplement troubleshooting results. \
                 Plumb indexes homepages and page sets, not the full text of the web, so search \
                 for names and topics, then open the page you need.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to search for." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_SEARCH_LIMIT, "description": "How many results to return (default 5, or 3 when the search has a direct answer such as a package card or an answer found before)." },
                    "country": country,
                    "language": { "type": "string", "description": "Optional language of the sites, a code such as en or de (default en); \"any\" for every language." },
                    "site": { "type": "string", "description": "Optional: only results on this site, a domain such as docs.python.org or github.com (like site: in the query): the pages Plumb has there, and a link into the site's own search for the rest." },
                    "kind": {
                        "type": "string",
                        "enum": KINDS.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
                        "description": format!(
                            "Optional: only one kind of result, best first: {}.",
                            KINDS
                                .iter()
                                .map(|(key, what)| format!("{key} ({what})"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    },
                    "after": { "type": "string", "description": "Paper publication on or after YYYY or YYYY-MM-DD; unknown dates are excluded. Requires kind=paper when a kind is supplied." },
                    "before": { "type": "string", "description": "Paper publication on or before YYYY or YYYY-MM-DD; day precision requires an actual publication day." },
                    "order": { "type": "string", "enum": ["relevance", "newest"], "description": "Paper ordering; newest orders within relevance tiers." },
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
        {
            "name": "facts",
            "title": "Facts",
            "description": "Facts about a country, place, person, company, book or film from \
                 Wikidata, by name (\"Australia\", \"Marie Curie\", \"Nvidia\"): capital, \
                 population, height, area, born, died, founded, founder, CEO, headquarters, \
                 currency, author, director, owner, spouse, head of state and the like. Each fact \
                 comes with the Wikidata item and property it is from, to cite. Use it instead of \
                 answering such facts from memory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "subject": { "type": "string", "description": "What the facts are about, by name." },
                    "about": { "type": "string", "description": "Optional: only one kind of fact (\"ceo\", \"population\", \"capital\")." },
                    "more_subjects": {
                        "type": "array",
                        "items": { "type": "string" },
                        "maxItems": MAX_FACT_SUBJECTS - 1,
                        "description": "Optional: more names to answer in the same call, each like subject (\"Canada\", \"Japan\"), to compare several things without a call for each.",
                    },
                    "country": country,
                },
                "required": ["subject"],
            },
            "annotations": read_only,
        },
    ]);
    if read_pages {
        let tools = tools.as_array_mut().expect("an array");
        tools.push(read_page_tool());
        point_search_at_read_page(tools);
    }
    if findings {
        tools
            .as_array_mut()
            .expect("an array")
            .push(report_finding_tool(share));
    }
    tools
}

/// `report_finding`'s description, with `share` and `share_query` when the
/// node shares findings.
fn report_finding_tool(share: bool) -> Value {
    let mut tool = json!({
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
    });
    if share {
        tool["description"] = json!(
            "Whenever a search led you to an answer, report it: what you searched for, the page \
             that answered it, why that page helped, and the answer itself. The next search for \
             the same thing on this computer lists it first (found_before), so no agent has to \
             work it out again. Kept on this node only, unless you set share: then agents \
             searching other Plumb nodes for the same thing see the page and why it helped (not \
             your search, answer or task), signed by this node."
        );
        let properties = &mut tool["inputSchema"]["properties"];
        properties["share"] = json!({ "type": "boolean", "description": "Also share the page and why it helped with other Plumb nodes, for agents searching for the same thing (default false). Only for public pages that would help anyone; never share anything private." });
        properties["share_query"] = json!({ "type": "boolean", "description": "With share: also share your search as you typed it (default false: only its words as numbers, for matching)." });
        tool["annotations"]["openWorldHint"] = json!(true);
    }
    tool
}

/// How `search`'s description ends without `read_page`...
const SEARCH_THEN_OPEN: &str = "then open the page you need.";

/// ...and with it.
const SEARCH_THEN_READ: &str = "then read a page with read_page.";

/// Has `search`'s description, among `tools`, send agents on to
/// `read_page`, once that is offered too.
pub(super) fn point_search_at_read_page(tools: &mut [Value]) {
    for tool in tools.iter_mut().filter(|tool| tool["name"] == "search") {
        if let Some(text) = tool["description"].as_str() {
            tool["description"] = json!(text.replace(SEARCH_THEN_OPEN, SEARCH_THEN_READ));
        }
    }
}

/// `read_page`'s description.
pub(super) fn read_page_tool() -> Value {
    json!({
        "name": "read_page",
        "title": "Read a page",
        "description": "Fetch a web page (web_fetch) and return its text, with headings and \
             lists marked in Markdown, without menus, ads or scripts. Use it after search or \
             official_site to read what a page says. Long pages come in parts: call again with \
             start set to next_start, or pass find to jump to the words you need. When the \
             first part of a long page lacks what you need, ask for its outline and read only \
             the section you need. Also says \
             whether the address is a look-alike of a better-known site.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The page's address." },
                "start": { "type": "integer", "minimum": 0, "description": "Character to start at, for the next part of a long page (default 0)." },
                "max_chars": { "type": "integer", "minimum": 200, "maximum": MAX_READ_CHARS, "description": "Most characters to return (default 6000)." },
                "links": { "type": "boolean", "description": "Also list the page's links (default false)." },
                "find": { "type": "string", "description": "Jump to the first place these words appear (from start), like Ctrl-F; says found: false when they do not." },
                "outline": { "type": "boolean", "description": "Return the page's headings, each with where it starts and its opening words, instead of its text (default false); then read a section with start. A page without headings returns its text." },
            },
            "required": ["url"],
        },
        "annotations": { "readOnlyHint": true, "idempotentHint": true, "openWorldHint": true },
    })
}

/// A required, non-empty text argument, cut to [`MAX_QUERY_CHARS`].
pub(super) fn text_arg(args: &Map<String, Value>, name: &str) -> Result<String, (i64, String)> {
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

/// An optional true-or-false argument, false when left out.
pub(super) fn bool_arg(args: &Map<String, Value>, name: &str) -> Result<bool, (i64, String)> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(on)) => Ok(*on),
        Some(_) => Err((INVALID_PARAMS, format!("{name} must be true or false"))),
    }
}

/// Longest URL a tool takes; longer than any real page's address.
pub(super) const MAX_URL_CHARS: usize = 4096;

/// A required URL argument: trimmed but otherwise as given, since
/// cutting or collapsing it would fetch some other page.
pub(super) fn url_arg(args: &Map<String, Value>, name: &str) -> Result<String, (i64, String)> {
    let url = args
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if url.is_empty() {
        return Err((INVALID_PARAMS, format!("{name} is required")));
    }
    if url.chars().count() > MAX_URL_CHARS {
        return Err((
            INVALID_PARAMS,
            format!("{name} is longer than {MAX_URL_CHARS} characters"),
        ));
    }
    Ok(url.to_string())
}

/// A whole number, also as a model may send it: `5.0` or `"5"`.
pub(super) fn as_whole(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| {
            value
                .as_f64()
                .filter(|n| *n >= 0.0 && n.fract() == 0.0 && *n < 1e15)
                .map(|n| n as u64)
        })
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

/// An optional whole-number argument.
pub(super) fn whole_number(
    args: &Map<String, Value>,
    name: &str,
) -> Result<Option<usize>, (i64, String)> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => as_whole(value)
            .map(|n| Some(usize::try_from(n).unwrap_or(usize::MAX)))
            .ok_or((INVALID_PARAMS, format!("{name} must be a whole number"))),
    }
}

pub(super) fn paper_text_arg<'a>(
    args: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, (i64, String)> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        _ => Err((INVALID_PARAMS, format!("{key} must be a string"))),
    }
}

/// `search`'s optional `site`: one domain, or a URL on it, whose results
/// alone are wanted.
pub(super) fn site_arg(args: &Map<String, Value>) -> Result<Option<String>, (i64, String)> {
    let Some(site) = args
        .get("site")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|site| !site.is_empty())
    else {
        return Ok(None);
    };
    let ops = plumb_core::Operators::parse(&format!("site:{site}"));
    match ops.sites.as_slice() {
        // "github" alone would be a top-level domain: a model means the site.
        [host] if host.contains('.') && !site.contains(char::is_whitespace) => {
            Ok(Some(host.clone()))
        }
        _ => Err((
            INVALID_PARAMS,
            format!("site must be one domain, such as docs.python.org; got {site:?}"),
        )),
    }
}

/// `search`'s optional `kind`: one of [`KINDS`].
pub(super) fn kind_arg(args: &Map<String, Value>) -> Result<Option<&'static str>, (i64, String)> {
    let Some(kind) = args
        .get("kind")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|kind| !kind.is_empty() && !kind.eq_ignore_ascii_case("any"))
    else {
        return Ok(None);
    };
    let kind = kind.to_lowercase();
    // "articles", "questions": as a model may write them.
    let key = kind
        .strip_suffix('s')
        .filter(|k| *k != "doc")
        .unwrap_or(&kind);
    KINDS
        .iter()
        .map(|(key, _)| *key)
        .find(|known| *known == key || *known == kind)
        .map(Some)
        .ok_or((
            INVALID_PARAMS,
            format!(
                "kind must be one of {}; got {kind:?}",
                KINDS
                    .iter()
                    .map(|(key, _)| *key)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ))
}

/// `facts`' optional `more_subjects`: names, each cut as `subject` is.
pub(super) fn more_subjects_arg(args: &Map<String, Value>) -> Result<Vec<String>, (i64, String)> {
    let names = match args.get("more_subjects") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(names)) => names,
        Some(_) => {
            return Err((
                INVALID_PARAMS,
                "more_subjects must be a list of names".to_string(),
            ))
        }
    };
    let names: Vec<String> = names
        .iter()
        .filter_map(Value::as_str)
        .map(|name| truncate_chars(&plumb_core::collapse_whitespace(name), MAX_QUERY_CHARS))
        .filter(|name| !name.is_empty())
        .collect();
    if names.len() >= MAX_FACT_SUBJECTS {
        return Err((
            INVALID_PARAMS,
            format!(
                "at most {} more_subjects in one call",
                MAX_FACT_SUBJECTS - 1
            ),
        ));
    }
    Ok(names)
}
