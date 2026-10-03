//! Reading a page's names and outbound links out of its HTML.
//!
//! The page is read as a stream of tags and text (html5ever's tokenizer)
//! and never built into a tree. Building the tree means repairing the markup
//! the way browsers do, and hostile markup makes that explode: each new
//! paragraph reopens every formatting element still open, so `<p>`, a
//! thousand `<b>`s and then `<p>x` over and over need gigabytes, and deep
//! nesting costs time quadratic in the depth. Reading the token stream takes
//! memory in proportion to the page, and time too, with one exception the
//! tokenizer has: it checks each attribute of a tag against all the tag's
//! earlier ones, so one tag with a hundred thousand attributes takes
//! minutes. Reading therefore stops at a time limit.

use std::cell::RefCell;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};
use plumb_core::{
    collapse_whitespace, normalize_text, registrable_domain, truncate_chars, MAX_TEXT_CHARS,
    SEARCH_TERMS,
};
use tracing::debug;
use url::Url;

use crate::{OutLink, PageMeta};

/// Most outbound links [`extract_page_meta`] keeps from one page.
pub const MAX_OUT_LINKS: usize = 500;

/// How long [`extract_page_meta`] keeps reading a page. Half a megabyte of
/// ordinary or merely messy HTML takes milliseconds; only markup built to be
/// slow comes near this, and the rest of such a page is skipped.
const READ_TIME_LIMIT: Duration = Duration::from_secs(2);

/// The page goes to the tokenizer in pieces of this many bytes, with the
/// time limit checked between them.
const READ_CHUNK_BYTES: usize = 4096;

/// Elements whose text never shows on the page.
const HIDDEN_ELEMENTS: &[&str] = &["script", "style", "noscript", "template", "iframe"];

/// Elements that begin a new line or cell on screen, so the text on either
/// side of them belongs to different words.
const WORD_BREAK_ELEMENTS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "caption",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "figcaption",
    "figure",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "td",
    "th",
    "tr",
    "ul",
];

/// Pulls title, meta description (`name="description"`, else
/// `og:description`), `og:site_name`, and cross-domain links out of a page.
/// Links are resolved against `base_url`, kept only for `http`/`https`
/// targets whose registrable domain differs from `base_url`'s, and their
/// text is the normalized anchor text, falling back to an image's `alt` or
/// the link's `aria-label`/`title`. Texts are whitespace-collapsed and cut to
/// [`plumb_core::MAX_TEXT_CHARS`]. At most 500 links are kept.
///
/// The page is read as a stream of tags and text, without building a tree,
/// so memory grows only in proportion to its length, however hostile the
/// markup. Reading stops after 2 seconds, keeping what was read by then;
/// only markup made to be slow takes that long.
///
/// Details:
/// - The title is the first `<title>` outside SVG and MathML (an SVG icon's
///   `<title>` never counts). Meta names are matched case-insensitively,
///   and the Open Graph keys are accepted in `name` as well as `property`.
///   Empty values count as missing.
/// - `<base href>` is ignored: relative links are the site's own pages.
/// - `javascript:`, `mailto:`, `tel:` and fragment-only (`#...`) links are
///   skipped, fragments and credentials are stripped from the kept URLs,
///   and links without a registrable domain (IP addresses, `localhost`)
///   are dropped. Exact repeats (same URL and text) are kept once.
/// - A link's text runs from its `<a>` to its `</a>` (or the next `<a>`)
///   and is read the way it shows on screen: script and style contents are
///   skipped, and block elements separate words, so
///   `<div>Acme</div><div>Bank</div>` gives `acme bank`.
pub fn extract_page_meta(base_url: &Url, html: &str) -> PageMeta {
    read_page(base_url, html, READ_TIME_LIMIT)
}

/// [`extract_page_meta`], reading for at most `time_limit`.
fn read_page(base_url: &Url, html: &str, time_limit: Duration) -> PageMeta {
    let page = RefCell::new(Page::new(base_url));
    let tokenizer = Tokenizer::new(Reader(&page), TokenizerOpts::default());
    let input = BufferQueue::default();
    let started = Instant::now();
    for chunk in chunks(html, READ_CHUNK_BYTES) {
        if started.elapsed() > time_limit {
            debug!("{base_url}: stopped reading the page after {time_limit:?}");
            break;
        }
        input.push_back(StrTendril::from_slice(chunk));
        // The reader never asks the tokenizer to pause (it runs no
        // scripts), so each call reads everything it was given.
        let _ = tokenizer.feed(&input);
    }
    tokenizer.end();
    drop(tokenizer);
    page.into_inner().into_meta()
}

/// `text` in pieces of `size` bytes (a little more where a character
/// straddles the cut).
fn chunks(mut text: &str, size: usize) -> impl Iterator<Item = &str> {
    std::iter::from_fn(move || {
        if text.is_empty() {
            return None;
        }
        let mut end = size.clamp(1, text.len());
        while !text.is_char_boundary(end) {
            end += 1;
        }
        let (chunk, rest) = text.split_at(end);
        text = rest;
        Some(chunk)
    })
}

/// Feeds the tokenizer's output to a [`Page`].
struct Reader<'p, 'a>(&'p RefCell<Page<'a>>);

impl TokenSink for Reader<'_, '_> {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        self.0.borrow_mut().read(token)
    }

    /// Inside SVG and MathML, `<![CDATA[...]]>` is text, not a comment.
    fn adjusted_current_node_present_but_not_in_html_namespace(&self) -> bool {
        self.0.borrow().foreign > 0
    }
}

/// What has been read of a page so far.
struct Page<'a> {
    base_url: &'a Url,
    own_domain: Option<String>,
    /// Whether the page title's `<title>` has been seen.
    title_seen: bool,
    /// The text of the page title's `<title>`, while it is being read.
    title_text: Option<String>,
    title: Option<String>,
    description: Option<String>,
    og_description: Option<String>,
    site_name: Option<String>,
    search_url: Option<String>,
    /// The GET form being read, while no search address has been found.
    form: Option<SearchForm>,
    /// The link being read, when it is one to keep.
    anchor: Option<Anchor>,
    links: Vec<OutLink>,
    seen: HashSet<(String, String)>,
    /// How many [`HIDDEN_ELEMENTS`] are open.
    hidden: usize,
    /// How many `<svg>` and `<math>` elements are open.
    foreign: usize,
}

/// A GET form on the page, between its `<form>` and its `</form>`.
struct SearchForm {
    action: Url,
    /// The name of its search box, once seen.
    terms: Option<String>,
    /// Hidden fields, sent along unchanged.
    fixed: Vec<(String, String)>,
}

/// Names of text boxes that hold search words, when not `type="search"`.
const SEARCH_BOX_NAMES: &[&str] = &[
    "q",
    "query",
    "s",
    "search",
    "k",
    "keyword",
    "keywords",
    "term",
    "terms",
    "searchTerm",
    "search_query",
    "searchterm",
    "text",
    "w",
    "_nkw",
    "st",
];

/// Most hidden fields kept from a search form.
const MAX_FIXED_FORM_FIELDS: usize = 4;

/// Longest search address kept.
const MAX_SEARCH_URL_BYTES: usize = 500;

/// A link to another site, between its `<a>` and its `</a>`.
struct Anchor {
    url: String,
    target_domain: String,
    /// The visible text so far.
    text: String,
    /// The first image `alt` with any text in it, normalized.
    alt: Option<String>,
    aria_label: Option<String>,
    title: Option<String>,
    /// [`Page::hidden`] at the `<a>`: text counts only while no hidden
    /// element has opened inside the link.
    hidden: usize,
}

impl<'a> Page<'a> {
    fn new(base_url: &'a Url) -> Self {
        Page {
            base_url,
            own_domain: registrable_domain(base_url.as_str()),
            title_seen: false,
            title_text: None,
            title: None,
            description: None,
            og_description: None,
            site_name: None,
            search_url: None,
            form: None,
            anchor: None,
            links: Vec::new(),
            seen: HashSet::new(),
            hidden: 0,
            foreign: 0,
        }
    }

    fn read(&mut self, token: Token) -> TokenSinkResult<()> {
        match token {
            Token::TagToken(tag) if tag.kind == TagKind::StartTag => return self.start_tag(&tag),
            Token::TagToken(tag) => self.end_tag(&tag),
            Token::CharacterTokens(text) => self.text(&text),
            Token::EOFToken => {
                self.close_anchor();
                self.close_title();
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }

    /// Handles a start tag, and tells the tokenizer how to read what follows.
    fn start_tag(&mut self, tag: &Tag) -> TokenSinkResult<()> {
        let name: &str = &tag.name;
        match name {
            "a" => {
                self.close_anchor();
                self.open_anchor(tag);
            }
            "img" => self.image(tag),
            "meta" => self.meta(tag),
            "form" => self.open_form(tag),
            "input" => self.input(tag),
            "svg" | "math" if !tag.self_closing => self.foreign += 1,
            "title" if self.foreign == 0 && !self.title_seen => {
                self.title_seen = true;
                self.title_text = Some(String::new());
            }
            _ => {}
        }
        if WORD_BREAK_ELEMENTS.contains(&name) {
            self.word_break();
        }
        // `<x/>` closes an element only in SVG and MathML.
        if self.foreign > 0 && tag.self_closing {
            return TokenSinkResult::Continue;
        }
        if HIDDEN_ELEMENTS.contains(&name) {
            self.hidden += 1;
        }
        contents_kind(name)
    }

    fn end_tag(&mut self, tag: &Tag) {
        let name: &str = &tag.name;
        match name {
            "a" => self.close_anchor(),
            "title" => self.close_title(),
            "form" => self.close_form(),
            "svg" | "math" => self.foreign = self.foreign.saturating_sub(1),
            _ => {}
        }
        if HIDDEN_ELEMENTS.contains(&name) {
            self.hidden = self.hidden.saturating_sub(1);
        }
        if WORD_BREAK_ELEMENTS.contains(&name) {
            self.word_break();
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(title) = &mut self.title_text {
            title.push_str(text);
        }
        if let Some(anchor) = self.visible_anchor() {
            anchor.text.push_str(text);
        }
    }

    fn word_break(&mut self) {
        if let Some(anchor) = self.visible_anchor() {
            anchor.text.push(' ');
        }
    }

    /// The open link, unless a hidden element has opened inside it.
    fn visible_anchor(&mut self) -> Option<&mut Anchor> {
        let hidden = self.hidden;
        self.anchor
            .as_mut()
            .filter(|anchor| anchor.hidden == hidden)
    }

    fn open_anchor(&mut self, tag: &Tag) {
        if self.links.len() >= MAX_OUT_LINKS {
            return;
        }
        let Some(url) = attr(tag, "href").and_then(|href| resolve_link(self.base_url, href)) else {
            return;
        };
        let Some(target_domain) = registrable_domain(url.as_str()) else {
            return;
        };
        if self.own_domain.as_deref() == Some(target_domain.as_str()) {
            return;
        }
        self.anchor = Some(Anchor {
            url: url.into(),
            target_domain,
            text: String::new(),
            alt: None,
            aria_label: attr(tag, "aria-label").map(str::to_string),
            title: attr(tag, "title").map(str::to_string),
            hidden: self.hidden,
        });
    }

    fn close_anchor(&mut self) {
        let Some(anchor) = self.anchor.take() else {
            return;
        };
        let link = OutLink {
            text: link_text(&anchor),
            url: anchor.url,
            target_domain: anchor.target_domain,
        };
        if self.links.len() < MAX_OUT_LINKS
            && self.seen.insert((link.url.clone(), link.text.clone()))
        {
            self.links.push(link);
        }
    }

    fn image(&mut self, tag: &Tag) {
        let Some(anchor) = self.visible_anchor() else {
            return;
        };
        if anchor.alt.is_some() {
            return;
        }
        let alt = clean_link_text(attr(tag, "alt").unwrap_or_default());
        if !alt.is_empty() {
            anchor.alt = Some(alt);
        }
    }

    fn close_title(&mut self) {
        if let Some(text) = self.title_text.take() {
            self.title = clean_text(&text);
        }
    }

    /// Takes the description (`name="description"`, else `og:description`)
    /// and `og:site_name`, each from the first tag with a non-empty value.
    fn meta(&mut self, tag: &Tag) {
        let Some(content) = attr(tag, "content") else {
            return;
        };
        let name = attr(tag, "name").unwrap_or_default().trim();
        let property = attr(tag, "property").unwrap_or_default().trim();
        // Open Graph belongs in `property`, but `name` is a common mistake.
        let is_og =
            |key: &str| name.eq_ignore_ascii_case(key) || property.eq_ignore_ascii_case(key);
        if self.description.is_none() && name.eq_ignore_ascii_case("description") {
            self.description = clean_text(content);
        }
        if self.og_description.is_none() && is_og("og:description") {
            self.og_description = clean_text(content);
        }
        if self.site_name.is_none() && is_og("og:site_name") {
            self.site_name = clean_text(content);
        }
    }

    /// Starts reading a form that submits with GET to this site.
    fn open_form(&mut self, tag: &Tag) {
        self.form = None;
        if self.search_url.is_some() || self.foreign > 0 {
            return;
        }
        let is_get = attr(tag, "method").is_none_or(|m| m.trim().eq_ignore_ascii_case("get"));
        let action = attr(tag, "action").unwrap_or_default().trim();
        let Ok(action) = self.base_url.join(action) else {
            return;
        };
        let same_site = matches!(action.scheme(), "http" | "https")
            && registrable_domain(action.as_str()).is_some()
            && registrable_domain(action.as_str()) == self.own_domain;
        if is_get && same_site {
            self.form = Some(SearchForm {
                action,
                terms: None,
                fixed: Vec::new(),
            });
        }
    }

    /// Notes a search box, or a hidden value sent along, in the open form.
    fn input(&mut self, tag: &Tag) {
        let Some(form) = &mut self.form else {
            return;
        };
        let Some(name) = attr(tag, "name").map(str::trim).filter(|n| !n.is_empty()) else {
            return;
        };
        let kind = attr(tag, "type")
            .unwrap_or("text")
            .trim()
            .to_ascii_lowercase();
        match kind.as_str() {
            "hidden" if form.fixed.len() < MAX_FIXED_FORM_FIELDS => {
                let value = attr(tag, "value").unwrap_or_default();
                form.fixed.push((name.to_string(), value.to_string()));
            }
            "search" => form.terms = Some(name.to_string()),
            "text" | "" if form.terms.is_none() && SEARCH_BOX_NAMES.contains(&name) => {
                form.terms = Some(name.to_string());
            }
            _ => {}
        }
    }

    /// Ends a form; one with a search box gives the site's search address.
    fn close_form(&mut self) {
        let Some(form) = self.form.take() else {
            return;
        };
        let Some(terms) = form.terms else {
            return;
        };
        let mut url = form.action;
        url.set_fragment(None);
        {
            let mut query = url.query_pairs_mut();
            query.clear();
            for (name, value) in &form.fixed {
                if *name != terms {
                    query.append_pair(name, value);
                }
            }
            query.append_pair(&terms, SEARCH_TERMS);
        }
        // The placeholder must survive the encoding of the query.
        let encoded: String =
            url::form_urlencoded::byte_serialize(SEARCH_TERMS.as_bytes()).collect();
        let template = url.as_str().replace(&encoded, SEARCH_TERMS);
        if template.len() <= MAX_SEARCH_URL_BYTES && template.matches(SEARCH_TERMS).count() == 1 {
            self.search_url = Some(template);
        }
    }

    fn into_meta(mut self) -> PageMeta {
        self.close_anchor();
        self.close_title();
        self.close_form();
        PageMeta {
            title: self.title,
            description: self.description.or(self.og_description),
            site_name: self.site_name,
            search_url: self.search_url,
            links: self.links,
        }
    }
}

/// How the tokenizer must read an element's contents: as text up to the
/// element's end tag for these elements (the way a browser's parser
/// switches it), as markup for the rest.
fn contents_kind(name: &str) -> TokenSinkResult<()> {
    match name {
        "script" => TokenSinkResult::RawData(RawKind::ScriptData),
        // `noscript` too, as in a browser with scripting on.
        "style" | "xmp" | "iframe" | "noembed" | "noframes" | "noscript" => {
            TokenSinkResult::RawData(RawKind::Rawtext)
        }
        "title" | "textarea" => TokenSinkResult::RawData(RawKind::Rcdata),
        "plaintext" => TokenSinkResult::Plaintext,
        _ => TokenSinkResult::Continue,
    }
}

/// The value of the tag's attribute `name` (lowercase).
fn attr<'t>(tag: &'t Tag, name: &str) -> Option<&'t str> {
    tag.attrs
        .iter()
        .find(|attr| &*attr.name.local == name)
        .map(|attr| &*attr.value)
}

/// Resolves an `href` to an absolute `http`/`https` URL without fragment or
/// credentials, or `None` for links that do not lead to a web page.
fn resolve_link(base_url: &Url, href: &str) -> Option<Url> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    if let Some((scheme, _)) = href.split_once(':') {
        let scheme = scheme.trim();
        if ["javascript", "mailto", "tel"]
            .iter()
            .any(|skip| scheme.eq_ignore_ascii_case(skip))
        {
            return None;
        }
    }
    let mut url = base_url.join(href).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_fragment(None);
    // These only fail for URLs that cannot carry credentials anyway.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Some(url)
}

/// The link's normalized text, falling back to an image's `alt`, then the
/// link's `aria-label` or `title`: logo and icon links often have no text.
fn link_text(anchor: &Anchor) -> String {
    let text = clean_link_text(&anchor.text);
    if !text.is_empty() {
        return text;
    }
    let labels = [&anchor.aria_label, &anchor.title]
        .into_iter()
        .flatten()
        .map(|label| clean_link_text(label));
    anchor
        .alt
        .clone()
        .into_iter()
        .chain(labels)
        .find(|text| !text.is_empty())
        .unwrap_or_default()
}

/// Collapses whitespace and cuts to [`MAX_TEXT_CHARS`]; `None` if nothing is left.
fn clean_text(text: &str) -> Option<String> {
    let text = truncate_chars(&collapse_whitespace(text), MAX_TEXT_CHARS);
    (!text.is_empty()).then_some(text)
}

fn clean_link_text(text: &str) -> String {
    truncate_chars(&normalize_text(text), MAX_TEXT_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_alloc::peak_bytes;

    fn extract(base: &str, html: &str) -> PageMeta {
        extract_page_meta(&Url::parse(base).unwrap(), html)
    }

    fn link(url: &str, target_domain: &str, text: &str) -> OutLink {
        OutLink {
            url: url.into(),
            target_domain: target_domain.into(),
            text: text.into(),
        }
    }

    #[test]
    fn reads_title_description_and_site_name() {
        let meta = extract(
            "https://www.usbank.com/",
            r#"<!DOCTYPE html><html><head>
                <meta charset="utf-8">
                <title>U.S. Bank | Personal Banking</title>
                <meta name="description" content="  Checking,
                    savings &amp; loans. ">
                <meta property="og:description" content="Not this one">
                <meta property="og:site_name" content="U.S. Bank">
            </head><body><h1>Welcome</h1></body></html>"#,
        );
        assert_eq!(meta.title.as_deref(), Some("U.S. Bank | Personal Banking"));
        assert_eq!(
            meta.description.as_deref(),
            Some("Checking, savings & loans.")
        );
        assert_eq!(meta.site_name.as_deref(), Some("U.S. Bank"));
        assert!(meta.links.is_empty());
    }

    #[test]
    fn reads_the_search_form() {
        let search = |html: &str| extract("https://www.shop.example/", html).search_url;
        assert_eq!(
            search(r#"<form action="/find" role="search"><input type="search" name="q"><button>Go</button></form>"#)
                .as_deref(),
            Some("https://www.shop.example/find?q={searchTerms}")
        );
        // Hidden fields go along; a plain text box with a search-like name counts.
        assert_eq!(
            search(
                r#"<form action="https://shop.example/s?old=1#top" method="GET">
                   <input type="hidden" name="cat" value="all &amp; more">
                   <input name="k"></form>"#
            )
            .as_deref(),
            Some("https://shop.example/s?cat=all+%26+more&k={searchTerms}")
        );
        // An unclosed form at the end of the page still counts.
        assert_eq!(
            search(r#"<form><input type=search name=query>"#).as_deref(),
            Some("https://www.shop.example/?query={searchTerms}")
        );
    }

    #[test]
    fn other_forms_are_not_search_forms() {
        let search = |html: &str| extract("https://www.shop.example/", html).search_url;
        for html in [
            // Posts, like a login form.
            r#"<form method="post" action="/s"><input type="search" name="q"></form>"#,
            // Sends to another site.
            r#"<form action="https://evil.example/s"><input type="search" name="q"></form>"#,
            r#"<form action="javascript:go()"><input type="search" name="q"></form>"#,
            // No search box: a newsletter sign-up.
            r#"<form action="/subscribe"><input type="email" name="email"><input name="name"></form>"#,
            // A search box outside any form.
            r#"<input type="search" name="q">"#,
        ] {
            assert_eq!(search(html), None, "{html}");
        }
        // The first search form wins.
        let two = r#"<form action="/a"><input type="search" name="q"></form>
                     <form action="/b"><input type="search" name="q"></form>"#;
        assert_eq!(
            search(two).as_deref(),
            Some("https://www.shop.example/a?q={searchTerms}")
        );
    }

    #[test]
    fn falls_back_to_open_graph_description() {
        let og_only = extract(
            "https://example.com/",
            r#"<meta property="og:description" content="From Open Graph">"#,
        );
        assert_eq!(og_only.description.as_deref(), Some("From Open Graph"));

        // An empty description does not count.
        let empty = extract(
            "https://example.com/",
            r#"<meta name="description" content="   ">
               <meta property="og:description" content="Fallback">"#,
        );
        assert_eq!(empty.description.as_deref(), Some("Fallback"));

        // Case-insensitive names, and Open Graph keys given as `name`.
        let loose = extract(
            "https://example.com/",
            r#"<META NAME="Description" CONTENT="Shouty">
               <meta name="og:site_name" content="Loose OG">"#,
        );
        assert_eq!(loose.description.as_deref(), Some("Shouty"));
        assert_eq!(loose.site_name.as_deref(), Some("Loose OG"));

        let nothing = extract("https://example.com/", "<p>No head at all</p>");
        assert_eq!(nothing, PageMeta::default());
    }

    #[test]
    fn keeps_only_links_to_other_domains() {
        let meta = extract(
            "https://www.example.com/",
            r#"
            <a href="https://example.com/a">apex</a>
            <a href="https://shop.example.com/">subdomain</a>
            <a href="HTTPS://WWW.Partner.ORG/Path">Partner</a>
            <a href="https://news.bbc.co.uk/">BBC News</a>
            <a href="http://192.168.1.1/">router</a>
            <a href="http://intranet/">intranet</a>
            <a href="https://xn--bcher-kva.de/">Bücher</a>
            "#,
        );
        assert_eq!(
            meta.links,
            [
                link("https://www.partner.org/Path", "partner.org", "partner"),
                link("https://news.bbc.co.uk/", "bbc.co.uk", "bbc news"),
                link("https://xn--bcher-kva.de/", "xn--bcher-kva.de", "bücher"),
            ]
        );
    }

    #[test]
    fn resolves_relative_links() {
        let meta = extract(
            "https://www.example.com/dir/page.html?x=1",
            r#"
            <base href="https://elsewhere.net/">
            <a href="/about">absolute path</a>
            <a href="other.html">relative path</a>
            <a href="../up">parent</a>
            <a href="?page=2">query only</a>
            <a href="//cdn.partner.org/logo">protocol-relative</a>
            <a href="  https://user:secret@friend.net/page#section  ">credentials</a>
            "#,
        );
        assert_eq!(
            meta.links,
            [
                link(
                    "https://cdn.partner.org/logo",
                    "partner.org",
                    "protocol relative"
                ),
                link("https://friend.net/page", "friend.net", "credentials"),
            ]
        );

        // A base without a registrable domain keeps every outside link.
        let meta = extract(
            "http://127.0.0.1:8080/",
            r#"<a href="/local">local</a><a href="https://example.org/">Example</a>"#,
        );
        assert_eq!(
            meta.links,
            [link("https://example.org/", "example.org", "example")]
        );
    }

    #[test]
    fn skips_links_that_are_not_web_pages() {
        let meta = extract(
            "https://example.com/",
            r##"
            <a href="javascript:void(0)">js</a>
            <a href="JavaScript:alert(1)">js upper</a>
            <a href=" javascript:alert(1)">js spaced</a>
            <a href="mailto:hello@other.org">mail</a>
            <a href="tel:+15551234567">phone</a>
            <a href="#top">fragment</a>
            <a href="">empty</a>
            <a>no href</a>
            <a href="ftp://files.other.org/">ftp</a>
            <a href="data:text/html,hi">data</a>
            <a href="https://kept.org/">kept</a>
            "##,
        );
        assert_eq!(meta.links, [link("https://kept.org/", "kept.org", "kept")]);
    }

    #[test]
    fn link_text_falls_back_to_alt_and_labels() {
        let meta = extract(
            "https://example.com/",
            r#"
            <a href="https://a.org/"><img src="logo.png" alt="A Corp"></a>
            <a href="https://b.org/"><img src="x.png" alt=""><img src="y.png" alt="B Inc."></a>
            <a href="https://c.org/" aria-label="C Group"><i class="icon"></i></a>
            <a href="https://d.org/" title="D Ltd"> </a>
            <a href="https://e.org/"><svg viewBox="0 0 1 1"><title>E Social</title></svg></a>
            <a href="https://f.org/" aria-label="Label loses">Text wins</a>
            <a href="https://g.org/"><img src="g.png"></a>
            "#,
        );
        let texts: Vec<(&str, &str)> = meta
            .links
            .iter()
            .map(|l| (l.target_domain.as_str(), l.text.as_str()))
            .collect();
        assert_eq!(
            texts,
            [
                ("a.org", "a corp"),
                ("b.org", "b inc"),
                ("c.org", "c group"),
                ("d.org", "d ltd"),
                ("e.org", "e social"),
                ("f.org", "text wins"),
                ("g.org", ""),
            ]
        );
    }

    #[test]
    fn link_text_reads_like_the_screen() {
        let meta = extract(
            "https://example.com/",
            r#"
            <a href="https://a.org/"><div>Acme</div><div>Bank</div></a>
            <a href="https://b.org/">Plumb<strong>Search</strong></a>
            <a href="https://c.org/">Line<br>break</a>
            <a href="https://d.org/">Shown<script>var hidden = 1;</script><style>.x{}</style></a>
            <a href="https://e.org/"><span>U.</span><span>S.</span> Bank</a>
            "#,
        );
        let texts: Vec<&str> = meta.links.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            ["acme bank", "plumbsearch", "line break", "shown", "us bank"]
        );
    }

    #[test]
    fn script_and_style_contents_are_not_markup() {
        let meta = extract(
            "https://example.com/",
            r#"<head><title>Real <b>title</b></title>
            <script>document.write('<title>Fake</title><a href="https://script.org/">x</a>');</script>
            <style>a::after { content: "<a href='https://style.org/'>"; }</style>
            <noscript><a href="https://noscript.org/">Enable JS</a></noscript>
            <template><a href="https://template.org/">Later</a></template>
            </head>
            <a href="https://kept.org/">Kept<noscript> (no JS)</noscript></a>
            <svg><style/><title/></svg><a href="https://after.org/">After SVG</a>"#,
        );
        assert_eq!(meta.title.as_deref(), Some("Real <b>title</b>"));
        let texts: Vec<(&str, &str)> = meta
            .links
            .iter()
            .map(|l| (l.target_domain.as_str(), l.text.as_str()))
            .collect();
        // A <template> holds markup: its links count, as they always have.
        assert_eq!(
            texts,
            [
                ("template.org", "later"),
                ("kept.org", "kept"),
                ("after.org", "after svg")
            ]
        );
    }

    #[test]
    fn copes_with_messy_html() {
        let meta = extract(
            "https://www.example.com/",
            "<HTML><HEAD><TITLE>Messy &amp; Co\n\t Home</TITLE>\
             <META NAME=description CONTENT='Unquoted &quot;and&quot; odd'>\
             <BODY><svg><title>Icon title</title></svg>\
             <P>Unclosed <B>bold <A HREF=https://other.org/x?a=1&amp;b=2>Other Org\
             <!-- comment --> <P><a href='https://third.net'>Third</a>\
             <div><a href=\"https://fourth.org\">Fourth</a></span></div></div></div>",
        );
        assert_eq!(meta.title.as_deref(), Some("Messy & Co Home"));
        assert_eq!(meta.description.as_deref(), Some(r#"Unquoted "and" odd"#));
        assert_eq!(
            meta.links,
            [
                link("https://other.org/x?a=1&b=2", "other.org", "other org"),
                link("https://third.net/", "third.net", "third"),
                link("https://fourth.org/", "fourth.org", "fourth"),
            ]
        );

        // An SVG <title> is not the page title, even when it comes first.
        let meta = extract(
            "https://example.com/",
            "<body><svg><title>Icon</title></svg><title>Real title</title></body>",
        );
        assert_eq!(meta.title.as_deref(), Some("Real title"));

        // Garbage in, nothing out, no panic.
        let meta = extract("https://example.com/", "<<<>>><a href=<title>\u{0}</a");
        assert_eq!(meta.title, None);
        assert!(meta.links.is_empty());

        // A link left open runs to the end of the page.
        let meta = extract(
            "https://example.com/",
            "<a href='https://open.org/'>Never <title>Late title</title> closed",
        );
        assert_eq!(meta.title.as_deref(), Some("Late title"));
        assert_eq!(
            meta.links,
            [link(
                "https://open.org/",
                "open.org",
                "never late title closed"
            )]
        );
    }

    #[test]
    fn cuts_long_texts_and_caps_links() {
        let long = "word ".repeat(200);
        let html = format!(
            r#"<title>{long}</title><meta name="description" content="{long}">
               <a href="https://long.org/">{long}</a>"#
        );
        let meta = extract("https://example.com/", &html);
        let title = meta.title.unwrap();
        assert_eq!(title.chars().count(), MAX_TEXT_CHARS - 1);
        assert!(title.starts_with("word word") && title.ends_with("word"));
        assert!(meta.description.unwrap().chars().count() <= MAX_TEXT_CHARS);
        assert!(meta.links[0].text.chars().count() <= MAX_TEXT_CHARS);

        let mut html = String::new();
        for i in 0..(MAX_OUT_LINKS + 100) {
            html.push_str(&format!(r#"<a href="https://site{i}.org/">Site {i}</a>"#));
        }
        let meta = extract("https://example.com/", &html);
        assert_eq!(meta.links.len(), MAX_OUT_LINKS);
        assert_eq!(meta.links[0].target_domain, "site0.org");
        assert_eq!(meta.links[MAX_OUT_LINKS - 1].target_domain, "site499.org");
    }

    #[test]
    fn exact_repeats_are_kept_once() {
        let meta = extract(
            "https://example.com/",
            r#"
            <a href="https://partner.org/">Partner</a>
            <a href="https://partner.org/#footer">partner</a>
            <a href="https://partner.org/">Partner Inc</a>
            <a href="https://partner.org/jobs">Partner</a>
            "#,
        );
        assert_eq!(
            meta.links,
            [
                link("https://partner.org/", "partner.org", "partner"),
                link("https://partner.org/", "partner.org", "partner inc"),
                link("https://partner.org/jobs", "partner.org", "partner"),
            ]
        );
    }

    #[test]
    fn deep_nesting_does_not_overflow_the_stack() {
        // Nothing recurses over the nesting, so a small stack will do.
        let depth = 100_000;
        let html = format!(
            r#"<a href="https://deep.org/">{}Deep{}</a>"#,
            "<span>".repeat(depth),
            "</span>".repeat(depth)
        );
        let meta = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || extract("https://example.com/", &html))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(meta.links, [link("https://deep.org/", "deep.org", "deep")]);
    }

    /// `head`, then `unit` over and over up to 512 KiB (the crawler's
    /// default page limit), then a link.
    fn hostile_page(head: &str, unit: &str) -> String {
        let mut html = head.to_string();
        while html.len() < 512 * 1024 {
            html.push_str(unit);
        }
        html + r#"<a href="https://survivor.org/">Survivor</a>"#
    }

    #[test]
    fn hostile_markup_takes_linear_time_and_memory() {
        // A tree builder reopens every open formatting element in each new
        // paragraph: with a thousand <b>s open that needs gigabytes.
        let mut reopening = String::from("<title>Hostile</title><p>");
        for i in 0..1000 {
            reopening.push_str(&format!("<b x={i}>"));
        }
        let pages = [
            hostile_page(&reopening, "<p>x"),
            // Nesting that a tree builder handles in quadratic time.
            hostile_page("<title>Hostile</title>", "<div>"),
            hostile_page(
                "<title>Hostile</title><a href='https://open.org/'>",
                "<a><b><i>x",
            ),
        ];
        let base = Url::parse("https://example.com/").unwrap();
        for html in pages {
            let started = Instant::now();
            // No time limit: each page must be read to its end.
            let (meta, peak) = peak_bytes(|| read_page(&base, &html, Duration::MAX));
            let took = started.elapsed();
            assert_eq!(meta.title.as_deref(), Some("Hostile"));
            assert!(
                meta.links.iter().any(|l| l.target_domain == "survivor.org"),
                "{:?}",
                meta.links
            );
            // A copy of the page and little else.
            assert!(peak < 4 << 20, "{peak} bytes at peak");
            assert!(took < Duration::from_secs(10), "took {took:?}");
        }
    }

    #[test]
    fn reading_stops_at_the_time_limit() {
        // One tag with 80,000 distinct attributes. The tokenizer compares
        // each attribute with all of the tag's earlier ones, which would
        // take minutes.
        let mut html = String::from("<title>Hostile</title><div");
        for i in 0..80_000 {
            html.push_str(&format!(" a{i}"));
        }
        html.push_str("><a href='https://survivor.org/'>Survivor</a>");

        let started = Instant::now();
        let (meta, peak) = peak_bytes(|| extract("https://example.com/", &html));
        let took = started.elapsed();
        // What came before the slow tag is kept.
        assert_eq!(meta.title.as_deref(), Some("Hostile"));
        assert!(peak < 16 << 20, "{peak} bytes at peak");
        // The limit, plus one piece of the page past it and a wide margin
        // for a busy machine.
        assert!(took < READ_TIME_LIMIT * 4, "took {took:?}");
    }

    #[test]
    fn pages_are_read_in_pieces() {
        let text = "aé€😀".repeat(1000);
        let pieces: Vec<&str> = chunks(&text, 7).collect();
        assert_eq!(pieces.concat(), text);
        // 7 bytes, or up to 3 more to finish a character; the last may be short.
        let (last, others) = pieces.split_last().unwrap();
        assert!(others.iter().all(|p| (7..=10).contains(&p.len())));
        assert!(last.len() <= 10);
        assert_eq!(chunks("", 7).count(), 0);

        // Cut anywhere, a page reads the same.
        let html =
            r#"<title>Caf&eacute; &amp; Co</title><a href="https://x.org/">X &copy; Org</a>"#;
        let base = Url::parse("https://example.com/").unwrap();
        let whole = extract_page_meta(&base, html);
        assert_eq!(whole.title.as_deref(), Some("Café & Co"));
        assert_eq!(whole.links[0].text, "x org");
        for size in 1..html.len() {
            let page = RefCell::new(Page::new(&base));
            let tokenizer = Tokenizer::new(Reader(&page), TokenizerOpts::default());
            let input = BufferQueue::default();
            for chunk in chunks(html, size) {
                input.push_back(StrTendril::from_slice(chunk));
                let _ = tokenizer.feed(&input);
            }
            tokenizer.end();
            drop(tokenizer);
            assert_eq!(page.into_inner().into_meta(), whole, "pieces of {size}");
        }
    }
}
