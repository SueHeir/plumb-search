//! Reading one page as plain text, for an AI assistant that picked it from
//! the results ([`PageReader`]). Nothing read is stored or indexed: Plumb
//! still crawls homepages only.
//!
//! The page is fetched the way the crawler fetches homepages, so it stays
//! off private networks: host names resolve to public addresses only, an
//! address written as an IP must be public too, and so must every redirect.
//! A page someone links to cannot get the reader to open the router's
//! admin page or a cloud metadata address.
//!
//! The HTML is read as a stream of tags and text, like
//! [`crate::extract_page_meta`] reads it, and turned into text with a little
//! Markdown: `#` before headings, `-` before list items, `|` between table
//! cells, and preformatted text kept as it is. Menus, footers, forms and
//! side bars are left out.

use std::cell::RefCell;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};
use reqwest::header::CONTENT_TYPE;
use reqwest::{redirect, Client};
use thiserror::Error;
use url::{Host, Url};

use crate::dns::{self, is_global};
use crate::extract::{
    attr, chunks, contents_kind, resolve_link, HIDDEN_ELEMENTS, READ_CHUNK_BYTES,
};

/// How [`PageReader`] fetches pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadConfig {
    pub user_agent: String,
    /// Whole-request time limit.
    pub timeout: Duration,
    /// Bytes of a page read; the rest is dropped.
    pub max_bytes: usize,
    /// Most redirects followed.
    pub max_redirects: usize,
    /// Lets pages on private networks be read (tests only, in practice).
    pub allow_private_addresses: bool,
    /// Host names connected to at these addresses, not looked up (tests
    /// only, in practice; the port is the URL's).
    pub resolve: Vec<(String, std::net::SocketAddr)>,
    /// Reads only pages on the web's own ports, 80 and 443, redirects
    /// included: a reader anyone may use must not knock on mail servers,
    /// databases and the like.
    pub web_ports_only: bool,
}

impl Default for ReadConfig {
    fn default() -> Self {
        ReadConfig {
            user_agent: crate::READ_USER_AGENT.to_string(),
            timeout: Duration::from_secs(20),
            max_bytes: 3 * 1024 * 1024,
            max_redirects: 8,
            allow_private_addresses: false,
            resolve: Vec::new(),
            web_ports_only: false,
        }
    }
}

/// A page read as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPage {
    /// Where the page was in the end, after redirects.
    pub url: String,
    pub title: Option<String>,
    /// The page's text, with a little Markdown.
    pub text: String,
    /// Links on the page, in order, each once: their text and address.
    pub links: Vec<(String, String)>,
    /// The page was longer than [`ReadConfig::max_bytes`] and was cut.
    pub cut: bool,
}

/// Why a page could not be read.
#[derive(Debug, Error)]
pub enum ReadError {
    #[error("{0:?} is not an http:// or https:// address")]
    NotWeb(String),
    #[error("{0} is on a private network, which is not read")]
    Private(String),
    #[error("{0} is not on port 80 or 443; this node reads only pages there")]
    Port(String),
    #[error("the page could not be fetched: {0}")]
    Fetch(String),
    #[error("the site answered {0}")]
    Status(u16),
    #[error("the page is {0}, which is not read; only web pages and plain text are")]
    NotText(String),
}

/// Fetches pages and reads them as text. One reader keeps one connection
/// pool, so make one and share it.
#[derive(Debug, Clone)]
pub struct PageReader {
    client: Client,
    cfg: ReadConfig,
}

impl PageReader {
    pub fn new(cfg: ReadConfig) -> reqwest::Result<Self> {
        let allow_private = cfg.allow_private_addresses;
        let max_redirects = cfg.max_redirects;
        let web_ports_only = cfg.web_ports_only;
        let mut client = Client::builder();
        for (name, addr) in &cfg.resolve {
            client = client.resolve(name, *addr);
        }
        let client = client
            .user_agent(cfg.user_agent.as_str())
            .timeout(cfg.timeout)
            .gzip(true)
            .redirect(redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() > max_redirects {
                    attempt.error("too many redirects")
                } else if !allow_private && names_private_ip(attempt.url()) {
                    attempt.error("a redirect to a private network")
                } else if web_ports_only && !on_web_port(attempt.url()) {
                    attempt.error("a redirect to a port other than 80 or 443")
                } else {
                    attempt.follow()
                }
            }))
            .dns_resolver(dns::Resolver::new(allow_private, 4))
            // Through a proxy, the resolver would see only the proxy's name.
            .no_proxy()
            .build()?;
        Ok(PageReader { client, cfg })
    }

    /// Fetches `address` and reads it as text.
    pub async fn read(&self, address: &str) -> Result<ReadPage, ReadError> {
        let url = web_url(address)?;
        if !self.cfg.allow_private_addresses && names_private_ip(&url) {
            return Err(ReadError::Private(url.host_str().unwrap_or("").to_string()));
        }
        if self.cfg.web_ports_only && !on_web_port(&url) {
            return Err(ReadError::Port(url.to_string()));
        }
        // Stack Exchange's sites turn away programs that read their pages,
        // but give the same question and answers through their API.
        if let Some((site, id)) = stack_exchange_question(&url) {
            if let Some(page) = self.read_stack_exchange(&url, site, id).await {
                return Ok(page);
            }
        }
        let mut response = self
            .client
            .get(url)
            .header(
                "accept",
                "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.1",
            )
            .send()
            .await
            .map_err(|err| fetch_error(&err))?;
        let status = response.status();
        if !status.is_success() {
            return Err(ReadError::Status(status.as_u16()));
        }
        let final_url = response.url().clone();
        let kind = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();
        let html = match kind.as_str() {
            "" | "text/html" | "application/xhtml+xml" => true,
            "text/plain" | "text/markdown" | "text/csv" | "application/json" => false,
            other => return Err(ReadError::NotText(other.to_string())),
        };
        let mut body = Vec::new();
        let mut cut = false;
        while let Some(chunk) = response.chunk().await.map_err(|err| fetch_error(&err))? {
            let room = self.cfg.max_bytes - body.len();
            if chunk.len() >= room {
                body.extend_from_slice(&chunk[..room]);
                cut = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        let body = String::from_utf8_lossy(&body);
        let mut page = if html {
            page_text(&final_url, &body)
        } else {
            ReadPage {
                url: String::new(),
                title: None,
                text: body.trim().to_string(),
                links: Vec::new(),
                cut: false,
            }
        };
        page.url = final_url.to_string();
        page.cut = cut;
        Ok(page)
    }
}

/// Answers kept of a Stack Exchange question, the best voted first.
const STACK_EXCHANGE_ANSWERS: usize = 4;
const STACK_EXCHANGE_API: &str = "https://api.stackexchange.com/2.3";

/// The Stack Exchange site (its API name) and question number of a
/// question's address: `stackoverflow`, 927358 for
/// `https://stackoverflow.com/questions/927358/how-do-i-undo-...`.
pub fn stack_exchange_question(url: &Url) -> Option<(&'static str, u64)> {
    let host = url.host_str()?.trim_start_matches("www.");
    let site = match host {
        "stackoverflow.com" => "stackoverflow",
        "mathoverflow.net" => "mathoverflow.net",
        host => plumb_core::stack_exchange::site_of(host)?.api,
    };
    let mut segments = url.path_segments()?;
    if !matches!(segments.next(), Some("questions" | "q")) {
        return None;
    }
    let id = segments.next()?.parse().ok()?;
    Some((site, id))
}

#[derive(serde::Deserialize)]
struct StackItems {
    #[serde(default)]
    items: Vec<StackPost>,
}

#[derive(serde::Deserialize)]
struct StackPost {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    score: i64,
    #[serde(default)]
    is_accepted: bool,
    #[serde(default)]
    tags: Vec<String>,
}

impl PageReader {
    /// The question `id` of the Stack Exchange `site` and its best answers,
    /// read as one page at `url`; `None` when the API cannot give them.
    async fn read_stack_exchange(&self, url: &Url, site: &str, id: u64) -> Option<ReadPage> {
        let get = |path: String| async move {
            let response = self
                .client
                .get(format!("{STACK_EXCHANGE_API}/{path}"))
                .send()
                .await
                .ok()?;
            if !response.status().is_success() {
                return None;
            }
            let bytes = response.bytes().await.ok()?;
            serde_json::from_slice::<StackItems>(&bytes).ok()
        };
        let question = get(format!("questions/{id}?site={site}&filter=withbody"))
            .await?
            .items
            .into_iter()
            .next()?;
        let answers = get(format!(
            "questions/{id}/answers?site={site}&filter=withbody&sort=votes&order=desc&pagesize={STACK_EXCHANGE_ANSWERS}"
        ))
        .await
        .map(|a| a.items)
        .unwrap_or_default();
        Some(stack_exchange_page(url, &question, &answers))
    }
}

/// A question and its answers as one page.
fn stack_exchange_page(url: &Url, question: &StackPost, answers: &[StackPost]) -> ReadPage {
    let title = question.title.as_deref().unwrap_or("Question");
    let mut html = format!("<title>{title}</title><h1>{title}</h1>");
    if !question.tags.is_empty() {
        html.push_str(&format!("<p>Tags: {}</p>", question.tags.join(", ")));
    }
    html.push_str(&question.body);
    if answers.is_empty() {
        html.push_str("<h2>No answers yet</h2>");
    }
    for answer in answers {
        html.push_str(&format!(
            "<h2>Answer{} (score {})</h2>",
            if answer.is_accepted { ", accepted" } else { "" },
            answer.score
        ));
        html.push_str(&answer.body);
    }
    let mut page = page_text(url, &html);
    page.url = url.to_string();
    page
}

fn fetch_error(err: &reqwest::Error) -> ReadError {
    // reqwest's own message hides the cause ("error sending request").
    let mut message = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(inner) = source {
        message = format!("{message}: {inner}");
        source = inner.source();
    }
    ReadError::Fetch(message)
}

/// `address` as an http or https URL, `https://` added when it has no
/// scheme ("example.com/page").
fn web_url(address: &str) -> Result<Url, ReadError> {
    let address = address.trim();
    let parsed = match Url::parse(address) {
        Ok(url) => Ok(url),
        Err(url::ParseError::RelativeUrlWithoutBase) => Url::parse(&format!("https://{address}")),
        Err(err) => Err(err),
    };
    let mut url = parsed.map_err(|_| ReadError::NotWeb(address.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ReadError::NotWeb(address.to_string()));
    }
    url.set_fragment(None);
    Ok(url)
}

/// Whether `url`'s host is an IP address that is not public. Names are
/// checked when they are looked up ([`dns::Resolver`]).
pub(crate) fn names_private_ip(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => !is_global(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => !is_global(IpAddr::V6(ip)),
        _ => false,
    }
}

/// Whether `url` is on port 80 or 443, said or implied by its scheme.
fn on_web_port(url: &Url) -> bool {
    matches!(url.port_or_known_default(), Some(80 | 443))
}

/// How long reading one page's HTML may take; see
/// [`crate::extract_page_meta`] on why there is a limit.
const READ_TIME_LIMIT: Duration = Duration::from_secs(3);

/// Most links kept from a page.
pub const MAX_READ_LINKS: usize = 200;

/// Elements left out of the text: menus, footers, forms and the like. A
/// `<header>` counts only outside `<main>` and `<article>`, where it is the
/// site's banner rather than the article's title.
const SKIPPED_ELEMENTS: &[&str] = &[
    "aside", "button", "dialog", "footer", "form", "nav", "select", "svg", "math",
];

/// Elements that start a new paragraph.
const PARAGRAPH_ELEMENTS: &[&str] = &[
    "address",
    "article",
    "blockquote",
    "details",
    "dl",
    "figure",
    "header",
    "main",
    "ol",
    "p",
    "section",
    "table",
    "ul",
];

/// Elements that start a new line.
const LINE_ELEMENTS: &[&str] = &[
    "br",
    "caption",
    "dd",
    "div",
    "dt",
    "figcaption",
    "hr",
    "summary",
    "tr",
];

/// Reads a page's title, text and links out of its HTML.
pub fn page_text(base_url: &Url, html: &str) -> ReadPage {
    let text = RefCell::new(TextReader::new(base_url));
    let tokenizer = Tokenizer::new(Sink(&text), TokenizerOpts::default());
    let input = BufferQueue::default();
    let started = Instant::now();
    for chunk in chunks(html, READ_CHUNK_BYTES) {
        if started.elapsed() > READ_TIME_LIMIT {
            break;
        }
        input.push_back(StrTendril::from_slice(chunk));
        let _ = tokenizer.feed(&input);
    }
    tokenizer.end();
    drop(tokenizer);
    text.into_inner().finish()
}

struct Sink<'p, 'a>(&'p RefCell<TextReader<'a>>);

impl TokenSink for Sink<'_, '_> {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        self.0.borrow_mut().read(token)
    }

    fn adjusted_current_node_present_but_not_in_html_namespace(&self) -> bool {
        self.0.borrow().skipped_foreign > 0
    }
}

struct TextReader<'a> {
    base_url: &'a Url,
    title: Option<String>,
    /// The `<title>`'s text while it is read.
    title_text: Option<String>,
    out: String,
    /// What goes before the next word: nothing, a space, a new line or a
    /// new paragraph (0 to 3).
    pending_break: u8,
    /// A prefix ("# ", "- ") for the next word.
    pending_prefix: Option<String>,
    hidden: usize,
    skipped: usize,
    /// `<svg>` and `<math>` open, whose CDATA is text.
    skipped_foreign: usize,
    /// `<main>` and `<article>` open.
    content: usize,
    /// `<header>`s open outside `content`, skipped.
    banner: usize,
    pre: usize,
    /// The link being read: its address and text.
    link: Option<(String, String)>,
    links: Vec<(String, String)>,
    /// Table cells seen in the current row.
    cells: usize,
}

impl<'a> TextReader<'a> {
    fn new(base_url: &'a Url) -> Self {
        TextReader {
            base_url,
            title: None,
            title_text: None,
            out: String::new(),
            pending_break: 0,
            pending_prefix: None,
            hidden: 0,
            skipped: 0,
            skipped_foreign: 0,
            content: 0,
            banner: 0,
            pre: 0,
            link: None,
            links: Vec::new(),
            cells: 0,
        }
    }

    fn shown(&self) -> bool {
        self.hidden == 0 && self.skipped == 0 && self.banner == 0
    }

    fn read(&mut self, token: Token) -> TokenSinkResult<()> {
        match token {
            Token::TagToken(tag) if tag.kind == TagKind::StartTag => return self.start(&tag),
            Token::TagToken(tag) => self.end(&tag),
            Token::CharacterTokens(text) => self.text(&text),
            _ => {}
        }
        TokenSinkResult::Continue
    }

    fn start(&mut self, tag: &Tag) -> TokenSinkResult<()> {
        let name: &str = &tag.name;
        if name == "title"
            && self.title.is_none()
            && self.title_text.is_none()
            && self.skipped_foreign == 0
        {
            self.title_text = Some(String::new());
        }
        if self.skipped_foreign > 0 && tag.self_closing {
            return TokenSinkResult::Continue;
        }
        if HIDDEN_ELEMENTS.contains(&name) {
            self.hidden += 1;
        }
        if SKIPPED_ELEMENTS.contains(&name) && !tag.self_closing {
            self.skipped += 1;
            if matches!(name, "svg" | "math") {
                self.skipped_foreign += 1;
            }
        }
        match name {
            "main" | "article" => self.content += 1,
            "header" if self.content == 0 => self.banner += 1,
            "pre" => self.pre += 1,
            _ => {}
        }
        if self.shown() {
            match name {
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    self.brk(3);
                    let level = usize::from(name.as_bytes()[1] - b'0');
                    self.pending_prefix = Some(format!("{} ", "#".repeat(level)));
                }
                "li" => {
                    self.brk(2);
                    self.pending_prefix = Some("- ".to_string());
                }
                "tr" => {
                    self.brk(2);
                    self.cells = 0;
                }
                "td" | "th" => {
                    if self.cells > 0 {
                        self.pending_prefix = Some("| ".to_string());
                        self.brk(1);
                    }
                    self.cells += 1;
                }
                "pre" => self.brk(3),
                "img" => {
                    if let Some(alt) = attr(tag, "alt").map(str::trim).filter(|a| !a.is_empty()) {
                        self.words(&format!("[image: {alt}]"));
                    }
                }
                _ if PARAGRAPH_ELEMENTS.contains(&name) => self.brk(3),
                _ if LINE_ELEMENTS.contains(&name) => self.brk(2),
                _ => {}
            }
            if name == "a" {
                self.close_link();
                self.link = attr(tag, "href")
                    .and_then(|href| resolve_link(self.base_url, href))
                    .map(|url| (url.to_string(), String::new()));
            }
        }
        contents_kind(name)
    }

    fn end(&mut self, tag: &Tag) {
        let name: &str = &tag.name;
        if name == "title" {
            if let Some(title) = self.title_text.take() {
                let title = plumb_core::collapse_whitespace(&title);
                if !title.is_empty() {
                    self.title = Some(title);
                }
            }
        }
        if name == "a" {
            self.close_link();
        }
        if HIDDEN_ELEMENTS.contains(&name) {
            self.hidden = self.hidden.saturating_sub(1);
        }
        if SKIPPED_ELEMENTS.contains(&name) {
            self.skipped = self.skipped.saturating_sub(1);
            if matches!(name, "svg" | "math") {
                self.skipped_foreign = self.skipped_foreign.saturating_sub(1);
            }
        }
        match name {
            "main" | "article" => self.content = self.content.saturating_sub(1),
            "header" if self.banner > 0 => self.banner -= 1,
            "pre" => {
                self.pre = self.pre.saturating_sub(1);
                self.brk(3);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.brk(3),
            _ if PARAGRAPH_ELEMENTS.contains(&name) => self.brk(3),
            _ if LINE_ELEMENTS.contains(&name) || name == "li" => self.brk(2),
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(title) = &mut self.title_text {
            title.push_str(text);
            return;
        }
        if !self.shown() {
            return;
        }
        if let Some((_, link_text)) = &mut self.link {
            link_text.push_str(text);
        }
        if self.pre > 0 {
            self.flush_break();
            self.out.push_str(text);
            return;
        }
        if text.starts_with(char::is_whitespace) {
            self.brk(1);
        }
        let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !words.is_empty() {
            self.words(&words);
        }
        if text.ends_with(char::is_whitespace) {
            self.brk(1);
        }
    }

    fn words(&mut self, words: &str) {
        self.flush_break();
        if let Some(prefix) = self.pending_prefix.take() {
            self.out.push_str(&prefix);
        }
        self.out.push_str(words);
    }

    fn brk(&mut self, kind: u8) {
        self.pending_break = self.pending_break.max(kind);
    }

    fn flush_break(&mut self) {
        if !self.out.is_empty() {
            match self.pending_break {
                0 => {}
                1 => self.out.push(' '),
                2 => self.out.push('\n'),
                _ => self.out.push_str("\n\n"),
            }
        }
        self.pending_break = 0;
    }

    fn close_link(&mut self) {
        if let Some((url, text)) = self.link.take() {
            let text = plumb_core::collapse_whitespace(&text);
            if !text.is_empty()
                && self.links.len() < MAX_READ_LINKS
                && !self.links.iter().any(|(_, seen)| *seen == url)
            {
                self.links.push((text, url));
            }
        }
    }

    fn finish(mut self) -> ReadPage {
        self.close_link();
        let text = self
            .out
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n");
        ReadPage {
            url: self.base_url.to_string(),
            title: self.title,
            text: text.trim().to_string(),
            links: self.links,
            cut: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(html: &str) -> ReadPage {
        page_text(&Url::parse("https://example.com/a/").unwrap(), html)
    }

    #[test]
    fn reads_are_not_sent_as_the_crawler() {
        let agent = ReadConfig::default().user_agent;
        assert!(agent.starts_with("plumb-mcp/"), "{agent}");
        assert!(agent.ends_with("(+https://github.com/SueHeir/plumb-search)"));
        assert!(!agent.contains(crate::ROBOTS_TOKEN), "{agent}");
    }

    #[test]
    fn reads_the_article_and_leaves_out_the_site_furniture() {
        let page = read(
            "<html><head><title> Pasta  recipe </title><style>p{x}</style></head><body>\
             <header><a href=/>Home</a> Menu</header><nav><a href=/x>Recipes</a></nav>\
             <main><article><header><h1>Cacio e pepe</h1></header>\
             <p>Cheese and <b>pepper</b>.\nServes  two.</p>\
             <h2>You need</h2><ul><li>Pecorino</li><li>Pepper <a href=\"/pepper#x\">(which)</a></li></ul>\
             <table><tr><th>Step</th><th>Time</th></tr><tr><td>Boil</td><td>10 min</td></tr></table>\
             <pre>  keep\n    this</pre><img alt=\"A bowl\" src=x.png>\
             <script>var hidden = 1;</script><form><button>Save</button></form></article></main>\
             <footer>Copyright</footer></body></html>",
        );
        assert_eq!(page.title.as_deref(), Some("Pasta recipe"));
        assert_eq!(
            page.text,
            "# Cacio e pepe\n\nCheese and pepper. Serves two.\n\n## You need\n\n- Pecorino\n\
             - Pepper (which)\n\nStep | Time\nBoil | 10 min\n\n  keep\n    this\n\n[image: A bowl]"
        );
        assert_eq!(
            page.links,
            [(
                "(which)".to_string(),
                "https://example.com/pepper".to_string()
            )]
        );
    }

    #[test]
    fn stack_exchange_questions_are_read_from_the_api() {
        let url = Url::parse("https://stackoverflow.com/questions/927358/how-do-i-undo").unwrap();
        assert_eq!(
            stack_exchange_question(&url),
            Some(("stackoverflow", 927358))
        );
        for other in [
            "https://stackoverflow.com/users/1/x",
            "https://example.com/questions/1",
            "https://stackoverflow.com/questions/tagged/rust",
        ] {
            assert_eq!(stack_exchange_question(&Url::parse(other).unwrap()), None);
        }
        let question = StackPost {
            title: Some("How do I undo the most recent local commits in Git?".into()),
            body: "<p>I committed the wrong files.</p>".into(),
            score: 27000,
            is_accepted: false,
            tags: vec!["git".into(), "undo".into()],
        };
        let answer = StackPost {
            title: None,
            body: "<pre><code>git reset HEAD~\n</code></pre>".into(),
            score: 30000,
            is_accepted: true,
            tags: Vec::new(),
        };
        let page = stack_exchange_page(&url, &question, &[answer]);
        assert_eq!(page.url, url.as_str());
        assert!(page.text.starts_with("# How do I undo"), "{}", page.text);
        assert!(
            page.text.contains("## Answer, accepted (score 30000)"),
            "{}",
            page.text
        );
        assert!(page.text.contains("git reset HEAD~"), "{}", page.text);
    }

    #[test]
    fn checks_addresses_before_fetching() {
        assert!(matches!(web_url("ftp://x.com"), Err(ReadError::NotWeb(_))));
        assert_eq!(
            web_url("example.com/page#top").unwrap().as_str(),
            "https://example.com/page"
        );
        for private in [
            "http://127.0.0.1/",
            "http://192.168.1.1/admin",
            "http://169.254.169.254/latest",
            "http://[::1]:8080/",
        ] {
            assert!(names_private_ip(&web_url(private).unwrap()), "{private}");
        }
        assert!(!names_private_ip(&web_url("https://1.1.1.1/").unwrap()));
        assert!(!names_private_ip(&web_url("https://example.com/").unwrap()));
    }

    #[tokio::test]
    async fn refuses_private_networks_and_reads_pages() {
        let reader = PageReader::new(ReadConfig::default()).unwrap();
        assert!(matches!(
            reader.read("http://127.0.0.1:9/").await,
            Err(ReadError::Private(_))
        ));
        // A name that resolves to a private address is refused too.
        assert!(matches!(
            reader.read("http://localhost:9/").await,
            Err(ReadError::Fetch(_))
        ));

        use axum::routing::get;
        let app = axum::Router::new()
            .route(
                "/",
                get(|| async {
                    axum::response::Html("<title>Hi</title><p>Hello <i>there</i></p>")
                }),
            )
            .route(
                "/moved",
                get(|| async { axum::response::Redirect::to("/") }),
            )
            .route(
                "/doc.pdf",
                get(|| async { ([("content-type", "application/pdf")], "%PDF") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let reader = PageReader::new(ReadConfig {
            allow_private_addresses: true,
            ..ReadConfig::default()
        })
        .unwrap();
        let page = reader.read(&format!("http://{addr}/moved")).await.unwrap();
        assert_eq!(page.url, format!("http://{addr}/"));
        assert_eq!(page.title.as_deref(), Some("Hi"));
        assert_eq!(page.text, "Hello there");
        assert!(matches!(
            reader.read(&format!("http://{addr}/doc.pdf")).await,
            Err(ReadError::NotText(kind)) if kind == "application/pdf"
        ));
        let small = PageReader::new(ReadConfig {
            allow_private_addresses: true,
            max_bytes: 10,
            ..ReadConfig::default()
        })
        .unwrap();
        let page = small.read(&format!("http://{addr}/")).await.unwrap();
        assert!(page.cut);
        let web_ports = PageReader::new(ReadConfig {
            allow_private_addresses: true,
            web_ports_only: true,
            ..ReadConfig::default()
        })
        .unwrap();
        assert!(matches!(
            web_ports.read(&format!("http://{addr}/")).await,
            Err(ReadError::Port(_))
        ));
    }

    #[test]
    fn web_ports_are_80_and_443() {
        let on = |url: &str| on_web_port(&Url::parse(url).unwrap());
        assert!(on("http://example.com/"));
        assert!(on("https://example.com/"));
        assert!(on("http://example.com:443/"));
        assert!(!on("http://example.com:25/"));
        assert!(!on("https://example.com:8443/"));
    }
}
