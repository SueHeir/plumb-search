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
use plumb_core::key_pages::{pick_key_pages, OwnLink};
use plumb_core::{
    collapse_whitespace, normalize_text, registrable_domain, truncate_chars, MAX_HEADINGS,
    MAX_HEADING_WORDS, MAX_TEXT_CHARS, SEARCH_TERMS,
};
use tracing::debug;

use crate::structured;
use url::Url;

use crate::terms::{pick_terms, TERM_WORDS};
use crate::{OutLink, PageMeta};

mod rich;
use rich::RichText;

/// Changes to rich extraction invalidate docs caches independently of
/// homepage metadata. Include this and the configuration in cache identity.
pub const DOCS_EXTRACTOR_VERSION: u32 = 1;

/// Rich content is explicitly enabled for inner docs pages. Homepage
/// callers retain their compact text and heading budgets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum InnerPageExtraction {
    #[default]
    Compact,
    Docs,
}

/// Most icon links [`extract_page_meta`] keeps from one page.
pub const MAX_ICONS: usize = 3;

/// Most `<link rel="icon">`s read from one page.
const MAX_ICON_LINKS: usize = 64;

/// Most outbound links [`extract_page_meta`] keeps from one page.
pub const MAX_OUT_LINKS: usize = 500;

/// Most links to the page's own site read for its key pages.
const MAX_OWN_LINKS: usize = 300;

/// Elements that hold a site's main menu, whose links can be key pages
/// without naming a [`plumb_core::PageIntent`].
const MENU_ELEMENTS: &[&str] = &["header", "nav"];

/// How long [`extract_page_meta`] keeps reading a page. Half a megabyte of
/// ordinary or merely messy HTML takes milliseconds; only markup built to be
/// slow comes near this, and the rest of such a page is skipped.
const READ_TIME_LIMIT: Duration = Duration::from_secs(2);

/// The page goes to the tokenizer in pieces of this many bytes, with the
/// time limit checked between them.
pub(crate) const READ_CHUNK_BYTES: usize = 4096;

/// Most words of the page's visible text [`extract_page_meta`] keeps.
pub const MAX_BODY_WORDS: usize = 100;

/// Most words of the page's visible text kept as [`PageMeta::page_text`],
/// for picking search terms from the whole page.
pub const MAX_PAGE_TEXT_WORDS: usize = 1000;

/// Bytes of visible text read before [`MAX_PAGE_TEXT_WORDS`] are surely in
/// hand (words average under ten bytes); the rest of the page's text is
/// skipped.
const BODY_TEXT_BYTES: usize = MAX_PAGE_TEXT_WORDS * 16;

/// Elements that hold a site's furniture (menus, banners, footers, forms)
/// rather than what the page is about; their text stays out of the body text.
const CHROME_ELEMENTS: &[&str] = &[
    "aside", "button", "dialog", "footer", "form", "header", "nav", "select",
];

/// Most section headings ([`PageMeta::sections`]) kept per page.
pub const MAX_SECTIONS: usize = 64;
/// Most words kept of each section heading.
pub const MAX_SECTION_WORDS: usize = 8;

/// Elements whose text never shows on the page.
pub(crate) const HIDDEN_ELEMENTS: &[&str] = &["script", "style", "noscript", "template", "iframe"];

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
/// - The search terms ([`PageMeta::terms`]) are picked from the title,
///   description, headings and page text with [`pick_terms`].
pub fn extract_page_meta(base_url: &Url, html: &str) -> PageMeta {
    extract_inner_page_meta(base_url, html, InnerPageExtraction::Compact)
}

/// Reads an inner page with the same tokenizer/time limit as homepages,
/// optionally retaining bounded symbols and passages from later sections.
pub fn extract_inner_page_meta(
    base_url: &Url,
    html: &str,
    extraction: InnerPageExtraction,
) -> PageMeta {
    let mut meta = read_page_with_extraction(base_url, html, READ_TIME_LIMIT, extraction);
    let text: Vec<&str> = meta
        .title
        .iter()
        .chain(&meta.description)
        .chain(&meta.headings)
        .chain(std::iter::once(&meta.page_text))
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect();
    meta.terms = pick_terms(&text.join(". "), TERM_WORDS);
    meta
}

/// [`extract_page_meta`], reading for at most `time_limit`.
#[cfg(test)]
fn read_page(base_url: &Url, html: &str, time_limit: Duration) -> PageMeta {
    read_page_with_extraction(base_url, html, time_limit, InnerPageExtraction::Compact)
}

fn read_page_with_extraction(
    base_url: &Url,
    html: &str,
    time_limit: Duration,
    extraction: InnerPageExtraction,
) -> PageMeta {
    let mut page = Page::new(base_url);
    if extraction == InnerPageExtraction::Docs {
        page.rich = Some(RichText::default());
    }
    let page = RefCell::new(page);
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
pub(crate) fn chunks(mut text: &str, size: usize) -> impl Iterator<Item = &str> {
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
    /// The `<html lang>` of the page, once its `<html>` is read.
    language: Option<String>,
    html_seen: bool,
    /// `<link rel="icon">` and the like, with their [`icon_rank`].
    icons: Vec<(u32, String)>,
    /// The first RSS or Atom `<link rel="alternate">`.
    feed: Option<String>,
    /// The text of the `<h1>` or `<h2>` being read, when it is visible.
    heading_text: Option<String>,
    headings: Vec<String>,
    /// Words in `headings`.
    heading_words: usize,
    /// The text of the `<h2>` or `<h3>` being read, when it is visible and
    /// outside [`CHROME_ELEMENTS`].
    section_text: Option<String>,
    sections: Vec<String>,
    /// Visible text outside [`CHROME_ELEMENTS`] and headings, up to
    /// [`BODY_TEXT_BYTES`].
    body: String,
    /// The text of the `<script type="application/ld+json">` being read.
    json_ld: Option<String>,
    /// JSON-LD blocks read so far, at most [`structured::MAX_BLOCKS`].
    json_ld_blocks: Vec<String>,
    /// The GET form being read, while no search address has been found.
    form: Option<SearchForm>,
    /// The link being read, when it is one to keep.
    anchor: Option<Anchor>,
    links: Vec<OutLink>,
    /// Links to the page's own site, for its key pages.
    own_links: Vec<OwnLink>,
    /// How many [`MENU_ELEMENTS`] are open.
    menu: usize,
    /// The page has a password box: it is a sign-in page.
    password_field: bool,
    seen: HashSet<(String, String)>,
    /// How many [`HIDDEN_ELEMENTS`] are open.
    hidden: usize,
    /// How many `<svg>` and `<math>` elements are open.
    foreign: usize,
    /// How many [`CHROME_ELEMENTS`] are open.
    chrome: usize,
    /// Whether a link to a spot on this page (`href="#..."`, such as "Skip
    /// to content") is open; its text stays out of the body text.
    in_page_link: bool,
    rich: Option<RichText>,
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
    /// A link to the page's own site, kept for its key pages instead.
    own: bool,
    /// Inside the site's main menu ([`MENU_ELEMENTS`]).
    in_menu: bool,
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
            language: None,
            html_seen: false,
            icons: Vec::new(),
            feed: None,
            heading_text: None,
            headings: Vec::new(),
            heading_words: 0,
            section_text: None,
            sections: Vec::new(),
            body: String::new(),
            json_ld: None,
            json_ld_blocks: Vec::new(),
            form: None,
            anchor: None,
            links: Vec::new(),
            own_links: Vec::new(),
            menu: 0,
            password_field: false,
            seen: HashSet::new(),
            hidden: 0,
            foreign: 0,
            chrome: 0,
            in_page_link: false,
            rich: None,
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
                self.close_heading();
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }

    /// Handles a start tag, and tells the tokenizer how to read what follows.
    fn start_tag(&mut self, tag: &Tag) -> TokenSinkResult<()> {
        if let Some(rich) = &mut self.rich {
            rich.start(tag);
        }
        let name: &str = &tag.name;
        match name {
            "a" => {
                self.close_anchor();
                self.in_page_link = attr(tag, "href").is_some_and(|h| h.trim().starts_with('#'));
                self.open_anchor(tag);
            }
            "html" if !self.html_seen => {
                self.html_seen = true;
                self.language = attr(tag, "lang").and_then(plumb_core::language_code);
            }
            "img" => self.image(tag),
            "meta" => self.meta(tag),
            "link" if self.foreign == 0 => self.link(tag),
            "form" => self.open_form(tag),
            "input" => {
                let password = attr(tag, "type")
                    .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("password"));
                self.password_field |= password && self.hidden == 0;
                self.input(tag);
            }
            "svg" | "math" if !tag.self_closing => self.foreign += 1,
            "h1" | "h2" | "h3" => {
                let shown = self.hidden == 0
                    && self.foreign == 0
                    && attr(tag, "hidden").is_none()
                    && !attr(tag, "aria-hidden").is_some_and(|v| v.eq_ignore_ascii_case("true"));
                if name != "h3" {
                    self.close_heading();
                    if shown {
                        self.heading_text = Some(String::new());
                    }
                }
                if name != "h1" {
                    self.close_section();
                    if shown && self.chrome == 0 {
                        self.section_text = Some(String::new());
                    }
                }
            }
            "script"
                if self.json_ld_blocks.len() < structured::MAX_BLOCKS
                    && attr(tag, "type").is_some_and(|kind| {
                        kind.trim().eq_ignore_ascii_case("application/ld+json")
                    }) =>
            {
                self.json_ld = Some(String::new());
            }
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
        if CHROME_ELEMENTS.contains(&name) {
            self.chrome += 1;
        }
        if MENU_ELEMENTS.contains(&name) {
            self.menu += 1;
        }
        contents_kind(name)
    }

    fn end_tag(&mut self, tag: &Tag) {
        if let Some(rich) = &mut self.rich {
            rich.end(tag);
        }
        let name: &str = &tag.name;
        match name {
            "a" => {
                self.close_anchor();
                self.in_page_link = false;
            }
            "title" => self.close_title(),
            "form" => self.close_form(),
            "script" => self.close_json_ld(),
            "h1" => self.close_heading(),
            "h2" => {
                self.close_heading();
                self.close_section();
            }
            "h3" => self.close_section(),
            "svg" | "math" => self.foreign = self.foreign.saturating_sub(1),
            _ => {}
        }
        if HIDDEN_ELEMENTS.contains(&name) {
            self.hidden = self.hidden.saturating_sub(1);
        }
        if CHROME_ELEMENTS.contains(&name) {
            self.chrome = self.chrome.saturating_sub(1);
        }
        if MENU_ELEMENTS.contains(&name) {
            self.menu = self.menu.saturating_sub(1);
        }
        if WORD_BREAK_ELEMENTS.contains(&name) {
            self.word_break();
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(rich) = &mut self.rich {
            rich.text(text);
        }
        if let Some(title) = &mut self.title_text {
            title.push_str(text);
        }
        if self.hidden == 0 {
            if let Some(heading) = &mut self.heading_text {
                if heading.len() < MAX_TEXT_CHARS * 4 {
                    heading.push_str(text);
                }
            }
            if let Some(section) = &mut self.section_text {
                if section.len() < MAX_TEXT_CHARS {
                    section.push_str(text);
                }
            }
        }
        if let Some(anchor) = self.visible_anchor() {
            anchor.text.push_str(text);
        }
        if let Some(json) = &mut self.json_ld {
            // One byte past the limit marks the block as too long to read.
            let room = (structured::MAX_BLOCK_BYTES + 1).saturating_sub(json.len());
            let mut end = room.min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            json.push_str(&text[..end]);
        }
        if self.body_open() {
            let room = BODY_TEXT_BYTES - self.body.len();
            let mut end = room.min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.body.push_str(&text[..end]);
        }
    }

    fn word_break(&mut self) {
        if let Some(anchor) = self.visible_anchor() {
            anchor.text.push(' ');
        }
        if let Some(heading) = &mut self.heading_text {
            heading.push(' ');
        }
        if let Some(section) = &mut self.section_text {
            section.push(' ');
        }
        if self.body_open() {
            self.body.push(' ');
        }
    }

    /// Whether text read now belongs to the body text: it shows on the
    /// page, is not the title, a heading or the site's furniture, and there
    /// is room.
    fn body_open(&self) -> bool {
        self.hidden == 0
            && self.foreign == 0
            && self.chrome == 0
            && !self.in_page_link
            && self.title_text.is_none()
            && self.heading_text.is_none()
            && self.body.len() < BODY_TEXT_BYTES
    }

    /// Keeps the heading just read, unless it repeats one or the headings
    /// are full; a heading cut by the word limit keeps its first words.
    fn close_heading(&mut self) {
        let Some(text) = self.heading_text.take() else {
            return;
        };
        let room = MAX_HEADING_WORDS.saturating_sub(self.heading_words);
        if self.headings.len() >= MAX_HEADINGS || room == 0 {
            return;
        }
        let words: Vec<&str> = text.split_whitespace().take(room).collect();
        let Some(heading) = clean_text(&words.join(" ")) else {
            return;
        };
        if self.headings.contains(&heading) {
            return;
        }
        self.heading_words += words.len();
        self.headings.push(heading);
    }

    /// Keeps the section heading just read, cut to [`MAX_SECTION_WORDS`]
    /// words, unless it repeats one or the sections are full.
    fn close_section(&mut self) {
        let Some(text) = self.section_text.take() else {
            return;
        };
        if self.sections.len() >= MAX_SECTIONS {
            return;
        }
        let words: Vec<&str> = text.split_whitespace().take(MAX_SECTION_WORDS).collect();
        let Some(section) = clean_text(&words.join(" ")) else {
            return;
        };
        if !self.sections.contains(&section) {
            self.sections.push(section);
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
        if self.links.len() >= MAX_OUT_LINKS && self.own_links.len() >= MAX_OWN_LINKS {
            return;
        }
        let Some(url) = attr(tag, "href").and_then(|href| resolve_link(self.base_url, href)) else {
            return;
        };
        let Some(target_domain) = registrable_domain(url.as_str()) else {
            return;
        };
        let own = self.own_domain.as_deref() == Some(target_domain.as_str());
        let full = if own {
            self.own_links.len() >= MAX_OWN_LINKS
        } else {
            self.links.len() >= MAX_OUT_LINKS
        };
        if full {
            return;
        }
        self.anchor = Some(Anchor {
            url: url.into(),
            target_domain,
            own,
            in_menu: self.menu > 0,
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
        if anchor.own {
            let label = [
                Some(&anchor.text),
                anchor.aria_label.as_ref(),
                anchor.title.as_ref(),
            ]
            .into_iter()
            .flatten()
            .map(|text| collapse_whitespace(text))
            .find(|text| !text.is_empty());
            if let Some(label) = label {
                self.own_links.push(OwnLink {
                    label,
                    url: anchor.url,
                    in_nav: anchor.in_menu,
                });
            }
            return;
        }
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

    fn close_json_ld(&mut self) {
        if let Some(json) = self.json_ld.take() {
            self.json_ld_blocks.push(json);
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

    /// Notes a `<link>` to the site's icon or feed.
    fn link(&mut self, tag: &Tag) {
        let rel = attr(tag, "rel").unwrap_or_default().to_ascii_lowercase();
        let kind = attr(tag, "type").unwrap_or_default();
        if self.feed.is_none() && is_feed_link(&rel, kind) {
            if let Some(url) = attr(tag, "href").and_then(|href| resolve_link(self.base_url, href))
            {
                self.feed = Some(url.into());
            }
        }
        if self.icons.len() >= MAX_ICON_LINKS {
            return;
        }
        let Some(href) = attr(tag, "href") else {
            return;
        };
        let Some(url) = resolve_link(self.base_url, href) else {
            return;
        };
        let sizes = attr(tag, "sizes").unwrap_or_default();
        if let Some(rank) = icon_rank(&rel, kind, sizes, url.path()) {
            self.icons.push((rank, url.into()));
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
        self.close_heading();
        self.close_section();
        self.close_form();
        self.close_json_ld();
        let words: Vec<&str> = self
            .body
            .split_whitespace()
            .take(MAX_PAGE_TEXT_WORDS)
            .collect();
        let body = &words[..words.len().min(MAX_BODY_WORDS)];
        PageMeta {
            body_text: (!body.is_empty()).then(|| body.join(" ")),
            page_text: words.join(" "),
            terms: Vec::new(),
            title: self.title,
            description: self.description.or(self.og_description),
            site_name: self.site_name,
            structured_names: structured::site_names(
                &self.json_ld_blocks,
                self.own_domain.as_deref(),
            ),
            search_url: self.search_url,
            language: self.language,
            icons: best_icons(self.icons),
            key_pages: match &self.own_domain {
                Some(domain) => pick_key_pages(
                    self.base_url.as_str(),
                    domain,
                    &self.own_links,
                    self.password_field,
                ),
                None => Vec::new(),
            },
            headings: self.headings,
            sections: self.sections,
            search: self.rich.and_then(RichText::finish),
            feed: self.feed,
            links: self.links,
        }
    }
}

/// Whether a `<link>` with `rel` (lowercased) and `kind` (its `type`)
/// points to the page's RSS or Atom feed.
fn is_feed_link(rel: &str, kind: &str) -> bool {
    let kind = kind.trim();
    rel.split_ascii_whitespace().any(|word| word == "alternate")
        && (kind.eq_ignore_ascii_case("application/rss+xml")
            || kind.eq_ignore_ascii_case("application/atom+xml"))
}

/// The page's icons, best first, each once, at most [`MAX_ICONS`]. A
/// stable sort keeps page order among equals.
fn best_icons(mut icons: Vec<(u32, String)>) -> Vec<String> {
    icons.sort_by_key(|(rank, _)| *rank);
    let mut best: Vec<String> = Vec::new();
    for (_, url) in icons {
        if best.len() == MAX_ICONS {
            break;
        }
        if !best.contains(&url) {
            best.push(url);
        }
    }
    best
}

/// How good a `<link>` is as the site's icon for a results page, lower
/// being better, or `None` when it is not a usable icon. `rel` is
/// lowercase. Results show icons at 16 to 32 pixels, so a square of 32 to
/// 256 pixels is best, then an icon of unknown size (often a 32-pixel
/// `.ico`), then the larger Apple touch icon, then very large or small
/// ones. SVG images and Safari's one-color `mask-icon`s are left out:
/// the crawler only reads bitmap formats.
fn icon_rank(rel: &str, kind: &str, sizes: &str, path: &str) -> Option<u32> {
    let words: Vec<&str> = rel.split_ascii_whitespace().collect();
    let icon = words.contains(&"icon");
    let touch = words
        .iter()
        .any(|w| *w == "apple-touch-icon" || *w == "apple-touch-icon-precomposed");
    if !icon && !touch {
        return None;
    }
    let svg = kind.trim().to_ascii_lowercase().starts_with("image/svg")
        || path.to_ascii_lowercase().ends_with(".svg")
        || sizes.trim().eq_ignore_ascii_case("any");
    if svg {
        return None;
    }
    let size = sizes
        .split_ascii_whitespace()
        .filter_map(|size| {
            let (w, h) = size
                .to_ascii_lowercase()
                .split_once('x')
                .map(|(w, h)| (w.parse::<u32>().ok(), h.parse::<u32>().ok()))?;
            Some(w?.min(h?))
        })
        .max();
    let rank = match (touch, size) {
        (false, Some(px)) if (32..=256).contains(&px) => px - 32,
        (false, None) => 300,
        (true, _) => 400,
        (false, Some(px)) if px > 256 => 500,
        (false, Some(px)) => 600 + (32 - px),
    };
    Some(rank)
}

/// How the tokenizer must read an element's contents: as text up to the
/// element's end tag for these elements (the way a browser's parser
/// switches it), as markup for the rest.
pub(crate) fn contents_kind(name: &str) -> TokenSinkResult<()> {
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
pub(crate) fn attr<'t>(tag: &'t Tag, name: &str) -> Option<&'t str> {
    tag.attrs
        .iter()
        .find(|attr| &*attr.name.local == name)
        .map(|attr| &*attr.value)
}

/// Resolves an `href` to an absolute `http`/`https` URL without fragment or
/// credentials, or `None` for links that do not lead to a web page.
pub(crate) fn resolve_link(base_url: &Url, href: &str) -> Option<Url> {
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
    fn rich_docs_keep_late_exact_symbols_and_source_anchors() {
        let intro = "ordinary introduction ".repeat(150);
        let html = format!(
            r##"<title>Node</title><main><p>{intro}</p>
            <h2 id="method-overview">Methods</h2><table><tr><td><a href="#class-node-method-set-multiplayer-authority">set_multiplayer_authority</a></td></tr></table>
            <h2 id="methods">Method descriptions</h2>
            <p class="classref-method" id="class-node-method-set-multiplayer-authority">void <strong>set_multiplayer_authority</strong>(id: int, recursive: bool = true)</p>
            <p>Sets this node's multiplayer authority to the peer with the given peer ID.</p>
            <h3 id="sort">Sorting</h3><pre><code>Array.prototype.sort() std::vector</code></pre></main>"##
        );
        let base =
            Url::parse("https://docs.godotengine.org/en/stable/classes/class_node.html").unwrap();
        let compact = extract_page_meta(&base, &html);
        let rich = extract_inner_page_meta(&base, &html, InnerPageExtraction::Docs);
        assert!(compact.search.is_none());
        assert_eq!(compact.body_text, rich.body_text);
        assert_eq!(compact.sections, rich.sections);
        assert!(!rich
            .body_text
            .as_deref()
            .unwrap()
            .contains("set_multiplayer_authority"));
        let search = rich.search.unwrap();
        for identifier in [
            "set_multiplayer_authority",
            "Array.prototype.sort",
            "std::vector",
        ] {
            assert!(
                search.symbols.iter().any(|s| s.identifier == identifier),
                "{identifier}: {search:?}"
            );
        }
        let authority = search
            .symbols
            .iter()
            .find(|s| s.identifier == "set_multiplayer_authority")
            .unwrap();
        assert_eq!(
            search
                .symbols
                .iter()
                .filter(|s| s.identifier == "set_multiplayer_authority")
                .count(),
            1
        );
        assert_eq!(
            authority.anchor.as_deref(),
            Some("class-node-method-set-multiplayer-authority")
        );
        assert!(search.text().contains("set multiplayer authority"));
        let definition = search
            .passages
            .iter()
            .find(|p| p.text.contains("given peer ID"))
            .unwrap();
        assert_eq!(
            definition.anchor.as_deref(),
            Some("class-node-method-set-multiplayer-authority")
        );
    }

    #[test]
    fn rich_docs_keep_troubleshooting_prose_beyond_the_body_limit() {
        let html = format!(
            r#"<title>Pod lifecycle</title><p>{}</p>
            <h2 id="container-restarts">Container restarts</h2>
            <p>The CrashLoopBackOff state indicates repeated container failures. Kubernetes applies an exponential backoff delay before restarting the container.</p>"#,
            "intro ".repeat(150)
        );
        let base = Url::parse("https://kubernetes.io/docs/concepts/workloads/pods/pod-lifecycle/")
            .unwrap();
        let meta = extract_inner_page_meta(&base, &html, InnerPageExtraction::Docs);
        assert!(!meta.body_text.unwrap().contains("CrashLoopBackOff"));
        let search = meta.search.unwrap();
        let passage = search
            .passages
            .iter()
            .find(|p| p.text.contains("exponential backoff"))
            .unwrap();
        assert_eq!(passage.heading, "Container restarts");
        assert_eq!(passage.anchor.as_deref(), Some("container-restarts"));
        assert!(search
            .symbols
            .iter()
            .any(|s| s.identifier == "CrashLoopBackOff"));
    }

    #[test]
    fn rich_docs_exclude_chrome_and_hidden_subtrees_even_with_bad_markup() {
        let html = r#"<nav><code>menu_symbol</code></nav><footer><h2>Footer</h2></footer>
            <div hidden><pre>hidden_symbol</pre></div>
            <div aria-hidden="true"><code>aria_symbol</code></div>
            <div style="display: NONE !important"><code>style_symbol</code></div>
            <script>script_symbol</script><svg><text>svg_symbol</text></svg><svg/><math/>
            <h2 id="visible">Visible</h2><p><code>visible_symbol</code> explains the supported API.
            <div hidden><span></nonsense>malformed_secret</div>"#;
        let base = Url::parse("https://example.com/docs").unwrap();
        let search = extract_inner_page_meta(&base, html, InnerPageExtraction::Docs)
            .search
            .unwrap();
        assert!(search.text().contains("visible_symbol"));
        for hidden in [
            "menu_symbol",
            "hidden_symbol",
            "aria_symbol",
            "style_symbol",
            "script_symbol",
            "svg_symbol",
            "malformed_secret",
        ] {
            assert!(!search.text().contains(hidden), "{hidden}: {search:?}");
        }
    }

    #[test]
    fn rich_docs_bounds_hold_on_huge_and_deep_markup() {
        use plumb_core::article::{MAX_SEARCH_BYTES, MAX_SEARCH_PASSAGES, MAX_SEARCH_SYMBOLS};
        let base = Url::parse("https://example.com/docs").unwrap();
        let html: String = (0..2000)
            .map(|i| {
                format!(
                    "<h2 id='section{i}'>Topic {i}</h2><p><code>api_symbol_{i}</code> {}</p>",
                    "原因 ".repeat(150)
                )
            })
            .collect();
        let (meta, peak) = peak_bytes(|| {
            read_page_with_extraction(&base, &html, Duration::MAX, InnerPageExtraction::Docs)
        });
        let search = meta.search.unwrap();
        assert!(search.symbols.len() <= MAX_SEARCH_SYMBOLS);
        assert!(search.passages.len() <= MAX_SEARCH_PASSAGES);
        assert!(serde_json::to_string(&search).unwrap().len() <= MAX_SEARCH_BYTES);
        assert!(peak < 4 << 20, "{peak} bytes at peak");
        assert!(search
            .symbols
            .iter()
            .any(|s| s.identifier != "api_symbol_0" && s.identifier != "api_symbol_1"));
        let deep = format!(
            "<p>Useful initial text.</p>{}<code>excluded_symbol</code>{}",
            "<span>".repeat(100_000),
            "</span>".repeat(100_000)
        );
        let (_, peak) = peak_bytes(|| {
            read_page_with_extraction(&base, &deep, Duration::MAX, InnerPageExtraction::Docs)
        });
        assert!(peak < 4 << 20, "{peak} bytes at peak");
    }

    #[test]
    fn rich_extraction_is_independent_of_tokenizer_chunk_boundaries() {
        let html = r#"<h2 id="std::vector">Vectors</h2><p><code>std::vector</code> and <strong>set_multiplayer_authority</strong> have exact identifiers.</p>"#;
        let base = Url::parse("https://example.com/docs").unwrap();
        let whole =
            read_page_with_extraction(&base, html, READ_TIME_LIMIT, InnerPageExtraction::Docs);
        for size in 1..html.len() {
            let mut page = Page::new(&base);
            page.rich = Some(RichText::default());
            let page = RefCell::new(page);
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

    #[test]
    fn reads_the_language_of_the_page() {
        let lang = |html: &str| extract("https://www.example.com/", html).language;
        assert_eq!(
            lang("<html lang=\"de-DE\"><title>x</title>").as_deref(),
            Some("de")
        );
        assert_eq!(
            lang("<html LANG=\"EN\"><title>x</title>").as_deref(),
            Some("en")
        );
        assert_eq!(lang("<html><title>x</title>"), None);
        assert_eq!(lang("<html lang=\"x-default\">"), None);
        // Only the page's own <html>.
        assert_eq!(lang("<html><svg><html lang=\"fr\"></svg>"), None);
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
    fn reads_body_text_without_furniture() {
        let meta = extract(
            "https://www.pnc.com/",
            r##"<html><head><title>PNC Bank</title>
                <style>h1 { color: red }</style><script>var x = "hidden";</script>
            </head><body>
                <header><a href="/">Logo</a><nav><a href="/login">Sign On</a></nav></header>
                <h1>Personal <b>Banking</b></h1>
                <form><button>Search</button></form>
                <a href="#main">Skip to content</a>
                <main id="main"><p>Checking accounts,<br>savings and loans.</p>
                    <div>Open an account</div><div>today.</div>
                    <svg><text>chart label</text></svg><noscript>Enable JS</noscript></main>
                <aside>Related</aside>
                <footer>Privacy | Careers</footer>
            </body></html>"##,
        );
        assert_eq!(meta.headings, ["Personal Banking"]);
        assert_eq!(
            meta.body_text.as_deref(),
            Some("Checking accounts, savings and loans. Open an account today.")
        );
    }

    #[test]
    fn reads_key_pages_from_links_to_the_site_itself() {
        let meta = extract(
            "https://www.paypal.com/us/home",
            r##"<html><body>
                <header><a href="/us/home">PayPal</a>
                    <nav><a href="/us/business">Business</a>
                    <a href="#menu">Menu</a>
                    <a href="https://developer.paypal.com/">Developer</a></nav>
                    <a href="/signin"> Log
                        In </a>
                    <a href="/us/webapps/mpp/account-selection">Sign Up</a></header>
                <main><a href="/us/cshelp/personal">Help</a>
                    <a href="https://paypal-login.example/">Log in here</a>
                    <a href="/story">Read how millions of people pay with PayPal every day</a></main>
                <footer><a href="/us/smarthelp/contact-us">Contact</a></footer>
            </body></html>"##,
        );
        let pages: Vec<(&str, &str)> = meta
            .key_pages
            .iter()
            .map(|p| (p.label.as_str(), p.url.as_str()))
            .collect();
        assert_eq!(
            pages,
            [
                ("Log In", "https://www.paypal.com/signin"),
                (
                    "Sign Up",
                    "https://www.paypal.com/us/webapps/mpp/account-selection"
                ),
                ("Developer", "https://developer.paypal.com/"),
                ("Help", "https://www.paypal.com/us/cshelp/personal"),
                ("Contact", "https://www.paypal.com/us/smarthelp/contact-us"),
                ("Business", "https://www.paypal.com/us/business"),
            ]
        );
        assert_eq!(meta.links.len(), 1, "other sites' links stay out links");
    }

    #[test]
    fn a_sign_in_homepage_gets_a_log_in_key_page() {
        let meta = extract(
            "https://www.facebook.com/",
            r#"<form method="post" action="/login/"><input name="email">
                <input type="password" name="pass"><button>Log in</button></form>
                <a href="/recover/initiate/">Forgotten password?</a>
                <a href="/r.php">Create new account</a>
                <footer><a href="/help/">Help</a></footer>"#,
        );
        let labels: Vec<&str> = meta.key_pages.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["Log in", "Create new account", "Help"]);
        assert_eq!(meta.key_pages[0].url, "https://www.facebook.com/");
    }

    #[test]
    fn body_text_stops_at_the_word_limit() {
        let words: Vec<String> = (0..1000).map(|i| format!("w{i}")).collect();
        let html = format!("<p>{}</p>", words.join(" "));
        let body = extract("https://example.com/", &html).body_text.unwrap();
        assert_eq!(body.split(' ').count(), MAX_BODY_WORDS);
        assert!(body.starts_with("w0 w1 w2"));
        assert!(body.ends_with(&format!("w{}", MAX_BODY_WORDS - 1)));

        let menu_only = extract(
            "https://example.com/",
            "<html><head><title>T</title></head><body><nav>Menu</nav></body></html>",
        );
        assert_eq!(menu_only.body_text, None);
    }

    #[test]
    fn reads_section_headings() {
        let meta = extract(
            "https://docs.python.org/3/tutorial/datastructures.html",
            r#"<header><h3>Navigation</h3></header>
                <h1>5. Data Structures</h1>
                <h2>5.1. More on <em>Lists</em></h2><p>Lists have methods.</p>
                <h3>5.1.3. List Comprehensions</h3><p>A concise way.</p>
                <h3 hidden>Not shown</h3>
                <h4>Too deep</h4>
                <h3>5.1.3. List Comprehensions</h3>"#,
        );
        assert_eq!(
            meta.sections,
            ["5.1. More on Lists", "5.1.3. List Comprehensions"]
        );
        assert_eq!(meta.headings, ["5. Data Structures", "5.1. More on Lists"]);
        let many: String = (0..100).map(|i| format!("<h3>Part {i}</h3>")).collect();
        assert_eq!(
            extract("https://example.com/", &many).sections.len(),
            MAX_SECTIONS
        );
    }

    #[test]
    fn reads_visible_headings() {
        let meta = extract(
            "https://www.navyfederal.org/",
            r#"<html><body>
                <h1>Navy Federal <span>Credit</span> Union</h1>
                <h2 hidden>Cookie settings</h2>
                <div style="display:none"></div>
                <h2>Checking<br>&amp; Savings</h2>
                <h3>Not this</h3>
                <h2>Checking &amp; Savings</h2>
                <template><h2>Nor this</h2></template>
                <svg><h2>Nor this</h2></svg>
                <h2>Auto loans
            </body></html>"#,
        );
        assert_eq!(
            meta.headings,
            [
                "Navy Federal Credit Union",
                "Checking & Savings",
                "Auto loans"
            ]
        );

        // Capped in number and in words.
        let many: String = (0..20).map(|i| format!("<h2>Heading {i}</h2>")).collect();
        let meta = extract("https://a.com/", &many);
        assert_eq!(meta.headings.len(), MAX_HEADINGS);
        let long = format!("<h1>{}</h1><h2>more</h2>", "word ".repeat(100));
        let meta = extract("https://a.com/", &long);
        assert_eq!(meta.headings.len(), 1);
        assert_eq!(meta.headings[0].split(' ').count(), MAX_HEADING_WORDS);
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
        assert_eq!(
            nothing,
            PageMeta {
                body_text: Some("No head at all".into()),
                page_text: "No head at all".into(),
                terms: pick_terms("No head at all", TERM_WORDS),
                ..PageMeta::default()
            }
        );
    }

    #[test]
    fn reads_the_site_names_in_json_ld() {
        let meta = extract(
            "https://www.example.com/",
            r#"<head>
            <script type="application/ld+json">
              {"@context": "https://schema.org", "@type": "Organization",
               "name": "Example Bank", "alternateName": "EXB",
               "url": "https://www.example.com/"}
            </script>
            <script type="Application/LD+JSON">[{"@type": "WebSite", "name": "Example"}]</script>
            <script>var notThis = {"@type": "WebSite", "name": "Script"};</script>
            </head><body><p>Hello</p></body>"#,
        );
        assert_eq!(meta.structured_names, ["Example Bank", "EXB", "Example"]);
        // The JSON stays out of the page's text.
        assert_eq!(meta.body_text.as_deref(), Some("Hello"));
    }

    #[test]
    fn notes_the_first_rss_or_atom_feed() {
        let meta = extract(
            "https://www.example.com/",
            r#"<head>
            <link rel="alternate" hreflang="fr" href="/fr/">
            <link rel="alternate" type="application/json" href="/feed.json">
            <link rel="Alternate" type="application/rss+xml" href="/rss.xml">
            <link rel="alternate" type="application/atom+xml" href="/atom.xml">
            </head>"#,
        );
        assert_eq!(
            meta.feed.as_deref(),
            Some("https://www.example.com/rss.xml")
        );
        let none = extract(
            "https://www.example.com/",
            r#"<link rel="alternate" type="application/rss+xml" href="javascript:x()">"#,
        );
        assert_eq!(none.feed, None);
    }

    #[test]
    fn picks_the_icons_best_for_a_results_page() {
        let meta = extract(
            "https://www.example.com/shop/",
            r#"<head>
            <link rel="apple-touch-icon" href="/touch.png">
            <link rel="mask-icon" href="/mask.svg" color="black">
            <link rel="icon" type="image/svg+xml" href="/icon.svg">
            <link rel="icon" href="/16.png" sizes="16x16">
            <link rel="shortcut icon" href="favicon.ico">
            <link rel="icon" href="https://cdn.example.net/64.png" sizes="64x64">
            <link rel="stylesheet" href="/site.css">
            <link rel="icon" href="javascript:alert(1)">
            </head>"#,
        );
        assert_eq!(
            meta.icons,
            [
                "https://cdn.example.net/64.png",
                "https://www.example.com/shop/favicon.ico",
                "https://www.example.com/touch.png",
            ]
        );
        let svg_only = extract(
            "https://example.com/",
            r#"<link rel="icon" href="/i.svg"><svg><link rel="icon" href="/in-svg.png"></svg>"#,
        );
        assert!(svg_only.icons.is_empty());
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
        let whole = read_page(&base, html, READ_TIME_LIMIT);
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
