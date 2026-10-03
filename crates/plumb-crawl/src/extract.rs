//! Reading a page's names and outbound links out of its HTML.

use std::collections::HashSet;

use plumb_core::{
    collapse_whitespace, normalize_text, registrable_domain, truncate_chars, MAX_TEXT_CHARS,
};
use scraper::{ElementRef, Html, Node, Selector};
use url::Url;

use crate::{OutLink, PageMeta};

/// Most outbound links [`extract_page_meta`] keeps from one page.
pub const MAX_OUT_LINKS: usize = 500;

/// Namespace of HTML elements; an SVG `<title>` is in another one.
const HTML_NAMESPACE: &str = "http://www.w3.org/1999/xhtml";

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
/// Details:
/// - The title is the first `<title>` in the HTML namespace (an SVG icon's
///   `<title>` never counts). Meta names are matched case-insensitively,
///   and the Open Graph keys are accepted in `name` as well as `property`.
///   Empty values count as missing.
/// - `<base href>` is ignored: relative links are the site's own pages.
/// - `javascript:`, `mailto:`, `tel:` and fragment-only (`#...`) links are
///   skipped, fragments and credentials are stripped from the kept URLs,
///   and links without a registrable domain (IP addresses, `localhost`)
///   are dropped. Exact repeats (same URL and text) are kept once.
/// - Link text is read the way it shows on screen: script and style
///   contents are skipped, and block elements separate words, so
///   `<div>Acme</div><div>Bank</div>` gives `acme bank`.
pub fn extract_page_meta(base_url: &Url, html: &str) -> PageMeta {
    let doc = Html::parse_document(html);
    let (description, site_name) = meta_names(&doc);
    PageMeta {
        title: page_title(&doc),
        description,
        site_name,
        links: out_links(&doc, base_url),
    }
}

fn page_title(doc: &Html) -> Option<String> {
    doc.select(&selector("title"))
        .find(|title| &*title.value().name.ns == HTML_NAMESPACE)
        .and_then(|title| clean_text(&title.text().collect::<String>()))
}

/// The description (`name="description"`, else `og:description`) and
/// `og:site_name`, each from the first tag with a non-empty value.
fn meta_names(doc: &Html) -> (Option<String>, Option<String>) {
    let mut description = None;
    let mut og_description = None;
    let mut site_name = None;
    for meta in doc.select(&selector("meta[content]")) {
        let meta = meta.value();
        let content = meta.attr("content").unwrap_or_default();
        let name = meta.attr("name").unwrap_or_default().trim();
        let property = meta.attr("property").unwrap_or_default().trim();
        // Open Graph belongs in `property`, but `name` is a common mistake.
        let is_og =
            |key: &str| name.eq_ignore_ascii_case(key) || property.eq_ignore_ascii_case(key);
        if description.is_none() && name.eq_ignore_ascii_case("description") {
            description = clean_text(content);
        }
        if og_description.is_none() && is_og("og:description") {
            og_description = clean_text(content);
        }
        if site_name.is_none() && is_og("og:site_name") {
            site_name = clean_text(content);
        }
    }
    (description.or(og_description), site_name)
}

fn out_links(doc: &Html, base_url: &Url) -> Vec<OutLink> {
    let images = selector("img[alt]");
    let own_domain = registrable_domain(base_url.as_str());
    let mut seen = HashSet::new();
    let mut links = Vec::new();
    for anchor in doc.select(&selector("a[href]")) {
        let href = anchor.value().attr("href").unwrap_or_default();
        let Some(url) = resolve_link(base_url, href) else {
            continue;
        };
        let Some(target_domain) = registrable_domain(url.as_str()) else {
            continue;
        };
        if own_domain.as_deref() == Some(target_domain.as_str()) {
            continue;
        }
        let link = OutLink {
            url: url.into(),
            target_domain,
            text: link_text(anchor, &images),
        };
        if seen.insert((link.url.clone(), link.text.clone())) {
            links.push(link);
            if links.len() == MAX_OUT_LINKS {
                break;
            }
        }
    }
    links
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
fn link_text(anchor: ElementRef<'_>, images: &Selector) -> String {
    let text = clean_link_text(&visible_text(anchor));
    if !text.is_empty() {
        return text;
    }
    let alts = anchor
        .select(images)
        .filter_map(|image| image.value().attr("alt"));
    let labels = ["aria-label", "title"]
        .into_iter()
        .filter_map(|attr| anchor.value().attr(attr));
    alts.chain(labels)
        .map(clean_link_text)
        .find(|text| !text.is_empty())
        .unwrap_or_default()
}

/// The text inside `element` as it reads on screen: text in
/// [`HIDDEN_ELEMENTS`] is skipped and [`WORD_BREAK_ELEMENTS`] separate
/// words. Walks the tree with its own stack, since a hostile page can nest
/// elements deeply enough to overflow the call stack.
fn visible_text(element: ElementRef<'_>) -> String {
    enum Step<'a> {
        Element(ElementRef<'a>),
        Text(&'a str),
        Space,
    }
    let mut text = String::new();
    let mut stack = vec![Step::Element(element)];
    while let Some(step) = stack.pop() {
        match step {
            Step::Text(chunk) => text.push_str(chunk),
            Step::Space => text.push(' '),
            Step::Element(element) => {
                let name = element.value().name();
                if HIDDEN_ELEMENTS.contains(&name) {
                    continue;
                }
                if WORD_BREAK_ELEMENTS.contains(&name) {
                    text.push(' ');
                    // Popped after all the children.
                    stack.push(Step::Space);
                }
                // Reversed, so the first child is popped first.
                for child in element.children().rev() {
                    match child.value() {
                        Node::Text(chunk) => stack.push(Step::Text(chunk)),
                        Node::Element(_) => {
                            stack.extend(ElementRef::wrap(child).map(Step::Element))
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    text
}

/// Collapses whitespace and cuts to [`MAX_TEXT_CHARS`]; `None` if nothing is left.
fn clean_text(text: &str) -> Option<String> {
    let text = truncate_chars(&collapse_whitespace(text), MAX_TEXT_CHARS);
    (!text.is_empty()).then_some(text)
}

fn clean_link_text(text: &str) -> String {
    truncate_chars(&normalize_text(text), MAX_TEXT_CHARS)
}

fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("built-in selectors are valid")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // A recursive walk of 10,000 levels would not fit in this small
        // stack. (Far deeper pages are slow to parse: html5ever is quadratic
        // in the nesting depth inside an `<a>`.)
        let depth = 10_000;
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
}
