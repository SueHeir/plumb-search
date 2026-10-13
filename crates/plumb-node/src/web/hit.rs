//! One result on a results page: a site with its key pages, a page, a
//! package or recent headlines.

use std::fmt::Write as _;

use plumb_core::{
    display_url, site_initial, truncate_chars, KeyPage, PageIntent, RecentNews, SafeSearch,
};
use plumb_index::pages::PageHit;
use plumb_index::{Hit, SearchOptions};

use super::api::NetworkResult;
use super::html::{escape_html, http_url, safe_href, time_ago};
use super::Icons;
use crate::country::country_name;
use crate::learn::Block;
use crate::news::Recent;

/// The `/go` link that notes a pick of `domain` for `query`.
pub(crate) fn go_link(query: &str, options: &SearchOptions, domain: &str) -> String {
    go_link_with(query, options, &[("d", domain)])
}

/// The `/go` link that notes a link of `block` was opened for `query`, and
/// goes on to `href`, one of the box's own links.
pub(crate) fn block_go_link(
    query: &str,
    options: &SearchOptions,
    block: Block,
    href: &str,
) -> String {
    go_link_with(query, options, &[("b", block.as_str()), ("u", href)])
}

pub(crate) fn go_link_with(query: &str, options: &SearchOptions, what: &[(&str, &str)]) -> String {
    let mut link = url::form_urlencoded::Serializer::new(String::new());
    link.append_pair("q", query);
    for (key, value) in what {
        link.append_pair(key, value);
    }
    link.append_pair("country", options.country.as_deref().unwrap_or("any"));
    if options.only_country {
        link.append_pair("only", "1");
    }
    if options.exact {
        link.append_pair("exact", "1");
    }
    append_filters(&mut link, options);
    format!("/go?{}", link.finish())
}

/// Adds safe search, when not the default, and the language filter (`any`
/// for none) to a link's parameters.
pub(crate) fn append_filters(
    params: &mut url::form_urlencoded::Serializer<String>,
    options: &SearchOptions,
) {
    if options.safe != SafeSearch::default() {
        params.append_pair("safe", options.safe.as_str());
    }
    params.append_pair("lang", options.language.as_deref().unwrap_or("any"));
    if options.recent != RecentNews::default() {
        params.append_pair("news", options.recent.as_str());
    }
}

/// The round badge before a result: the site's icon, or else the first
/// letter of its name on a color of its own. `icon` is a `data:` URL.
pub(crate) fn site_badge(domain: &str, icon: Option<&str>) -> String {
    match icon {
        Some(icon) => format!(
            "<span class=\"ic\"><img src=\"{}\" alt=\"\" width=\"18\" height=\"18\"></span>",
            escape_html(icon)
        ),
        None => {
            let (letter, color) = site_initial(domain);
            format!(
                "<span class=\"ic l{color}\" aria-hidden=\"true\">{}</span>",
                escape_html(&letter.to_string())
            )
        }
    }
}

/// One result: the site's badge, domain and address above its name, then
/// its description. A site that came from other nodes (`network`) is
/// tinted and says so. `go` is the `/go` link to send the click through
/// instead of linking to the site directly; `icon` is the site's icon as a
/// `data:` URL.
/// A single page (a Wikipedia article, a GitHub repository) listed among
/// the sites, with its set's icon.
pub(crate) fn render_page(out: &mut String, hit: &PageHit, icon: Option<&str>) {
    let Some(href) = http_url(&hit.page.url) else {
        return;
    };
    let badge = site_badge(hit.page.set_domain(), icon);
    let _ = write!(
        out,
        "<li class=\"pg\"><a class=\"r\" href=\"{}\" rel=\"noreferrer\"><span class=\"site\">{badge}\
         <span class=\"sn\"><span class=\"dn\">{}</span><span class=\"u\">{}</span></span></span>\
         <span class=\"t\">{}</span></a>",
        escape_html(&href),
        escape_html(hit.page.set_name()),
        escape_html(&display_url(&href)),
        escape_html(&truncate_chars(&hit.page.title, 150)),
    );
    if let Some(description) = hit
        .page
        .description
        .as_deref()
        .filter(|d| !d.trim().is_empty())
    {
        let _ = write!(out, "<p class=\"d\">{}</p>", escape_html(description));
    }
    if let Some(package) = &hit.page.package {
        render_package(out, package);
    }
    if hit.page.set == plumb_index::pages::PAPERS_SET {
        let parts = hit
            .page
            .paper
            .as_ref()
            .map(|paper| {
                let mut parts: Vec<_> = paper
                    .date_labels()
                    .iter()
                    .map(|(label, date)| format!("{label} {}", escape_html(date)))
                    .collect();
                parts.push(format!("{} {}", paper.count, paper.count_label()));
                parts
            })
            .unwrap_or_else(|| {
                vec![
                    "Published unknown".to_string(),
                    format!("{} popularity (count type unknown)", hit.page.views),
                ]
            });
        let _ = write!(out, "<p class=\"d pk\">{}</p>", parts.join(" &middot; "));
    }
    if let Some(href) = hit.page.free_copy().and_then(http_url) {
        let host = display_url(&href);
        let host = host.split('/').next().unwrap_or(&host);
        let _ = write!(
            out,
            "<p class=\"d pk\"><a href=\"{}\" rel=\"noreferrer\">Free to read</a> on {}</p>",
            escape_html(&href),
            escape_html(host)
        );
    }
    let _ = writeln!(
        out,
        "<div class=\"m rank-meta\"><span title=\"{} {}\">score {:.3}</span></div></li>",
        hit.page
            .paper
            .as_ref()
            .filter(|_| hit.page.set == plumb_index::pages::PAPERS_SET)
            .map_or(hit.page.views, |p| p.count),
        match hit.page.set.as_str() {
            plumb_index::pages::GITHUB_SET => "stars",
            plumb_index::pages::BOOKS_SET => "readers",
            plumb_index::pages::PODCASTS_SET =>
                "popularity (score, then listing, years and episodes)",
            plumb_index::pages::MUSIC_SET => "listeners on ListenBrainz",
            plumb_index::pages::FILMS_SET => "sitelinks on Wikidata",
            plumb_index::pages::DOCS_SET => "the docs site's weight over the page's depth",
            plumb_index::pages::REFERENCE_SET | plumb_index::pages::SUBPAGES_SET => {
                "the site's weight over the page's depth"
            }
            plumb_index::pages::WANTED_SET => "a thousand over the page's depth",
            plumb_index::pages::PAPERS_SET => hit
                .page
                .paper
                .as_ref()
                .map_or("popularity (count type unknown)", |p| p.count_label()),
            plumb_index::pages::PACKAGES_SET => "use (share of the registry's most, in billionths)",
            _ => "views",
        },
        hit.score
    );
}

/// A package's card under its result: its latest version, the command
/// that installs it, and links to its docs and code.
fn render_package(out: &mut String, package: &plumb_core::packages::PackageInfo) {
    let mut parts: Vec<String> = Vec::new();
    if let Some(version) = &package.version {
        let mut latest = format!("Latest {}", escape_html(version));
        if let Some(released) = &package.released {
            let _ = write!(latest, " ({})", escape_html(released));
        }
        parts.push(latest);
    }
    if let Some(license) = &package.license {
        parts.push(escape_html(license));
    }
    if let Some(install) = package.install() {
        parts.push(format!("<code>{}</code>", escape_html(&install)));
    }
    for (label, url) in [
        ("Docs", package.docs()),
        ("Code", package.repo.clone()),
        ("Home", package.homepage.clone()),
    ] {
        if let Some(href) = url.as_deref().and_then(http_url) {
            parts.push(format!(
                "<a href=\"{}\" rel=\"noreferrer\">{label}</a>",
                escape_html(&href)
            ));
        }
    }
    if !parts.is_empty() {
        let _ = write!(out, "<p class=\"d pk\">{}</p>", parts.join(" &middot; "));
    }
}

/// The "Recent" block, as an item of the results list: the latest posts
/// of the site the query names, or recent headlines about its words, each
/// with its site and age, folded behind a one-line summary unless `view`
/// says open (no script needed: `<details>`). Feed text is as untrusted
/// as any record's, and is escaped the same way.
pub(crate) fn render_recent(
    recent: &Recent,
    view: RecentNews,
    now: u64,
    link: &dyn Fn(&str) -> String,
) -> String {
    let heading = match &recent.site {
        Some(site) => format!("Latest from {}", escape_html(site)),
        None => "Recent".to_string(),
    };
    if recent.headlines.is_empty() {
        let message = if recent.status == "source_unavailable" {
            "No current indexed headlines available from this publisher's feeds."
        } else {
            "No indexed headlines match the requested constraints."
        };
        return format!(
            "<li class=\"news\"><span class=\"nh\">{heading}</span> \
            <span class=\"m\">{message}</span></li>\n"
        );
    }
    let mut items = String::new();
    let mut shown = 0;
    for headline in &recent.headlines {
        let Some(href) = http_url(&headline.url) else {
            continue;
        };
        shown += 1;
        let _ = write!(
            items,
            "<li><a href=\"{}\" rel=\"noreferrer\">{}</a><div class=\"m\">{} &middot; {}</div></li>",
            escape_html(&link(&href)),
            escape_html(&truncate_chars(&headline.title, 150)),
            escape_html(&headline.domain),
            time_ago(headline.at, now)
        );
    }
    let newest = recent.headlines.iter().map(|h| h.at).max().unwrap_or(now);
    let count = if shown == 1 {
        "1 headline".to_string()
    } else {
        format!("{shown} headlines")
    };
    format!(
        "<li class=\"news\"><details{}><summary><span class=\"nh\">{heading}</span> \
         <span class=\"m\">{count}, newest {}</span></summary><ol>{items}</ol></details></li>\n",
        if view == RecentNews::Expanded {
            " open"
        } else {
            ""
        },
        time_ago(newest, now)
    )
}

/// [`render_page`], with what plugins say about it from `marked`.
pub(crate) fn render_marked_page(
    out: &mut String,
    hit: &PageHit,
    icons: &Icons,
    marked: &dyn Fn(Option<String>) -> String,
) {
    let mut rendered = String::new();
    render_page(&mut rendered, hit, icons.get(hit.page.set_domain()));
    let line = marked(http_url(&hit.page.url));
    if let Some(end) = rendered.rfind("</li>") {
        rendered.insert_str(end, &line);
    }
    out.push_str(&rendered);
}

/// Most key pages listed under a result.
const SHOWN_KEY_PAGES: usize = 6;

/// The part of the site `domain` that the query names, from the article
/// carried under its result: YouTube Music's `https://music.youtube.com/`
/// for "youtube music" or "music youtube", when that article's item has
/// it as its official website. The query must have the words of the
/// article's title or one of its other names, in any order.
pub(crate) fn product_of(page: &PageHit, domain: &str, query: &str) -> Option<KeyPage> {
    let website = page.page.website.as_deref()?;
    let href = http_url(website)?;
    if plumb_core::registrable_domain(&href)? != domain {
        return None;
    }
    let words = |text: &str| {
        let mut words: Vec<String> = plumb_core::normalize_text(text)
            .split(' ')
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect();
        words.sort_unstable();
        words
    };
    let asked = words(query);
    let title = page.page.title.as_str();
    let base = match title.rfind(" (") {
        Some(i) if title.ends_with(')') && i > 0 => &title[..i],
        _ => title,
    };
    let named = std::iter::once(base)
        .chain(page.page.aliases.iter().map(String::as_str))
        .any(|name| !asked.is_empty() && words(name) == asked);
    named.then(|| KeyPage {
        label: base.to_string(),
        url: href,
    })
}

/// The site's key pages (sign in, docs, pricing) under the top result,
/// when the query names that site: on its own ("paypal"), or followed by
/// what is wanted from it ("paypal login"), whose page then comes first,
/// in bold. A part of the site the query names (`product`, from
/// [`product_of`]) comes first, in bold, under any result. Empty
/// otherwise, and when the site has fewer than two key pages and none for
/// what was asked.
pub(crate) fn key_pages_line(hit: &Hit, query: &str, product: Option<&KeyPage>) -> String {
    if !hit.named && product.is_none() {
        return String::new();
    }
    let mut pages: Vec<(&KeyPage, String)> = hit
        .key_pages
        .iter()
        .filter(|_| hit.named)
        .filter(|page| page.is_valid_for(&hit.domain))
        .filter_map(|page| Some((page, http_url(&page.url)?)))
        .take(SHOWN_KEY_PAGES)
        .collect();
    let wanted = PageIntent::of_query_end(query).map(|(intent, _)| intent);
    let mut matched = wanted.and_then(|wanted| {
        pages
            .iter()
            .position(|(page, _)| page.intent() == Some(wanted))
    });
    if let Some(at) = matched {
        let page = pages.remove(at);
        pages.insert(0, page);
    }
    if let Some(product) = product {
        pages.retain(|(_, href)| *href != product.url);
        pages.insert(0, (product, product.url.clone()));
        pages.truncate(SHOWN_KEY_PAGES);
        matched = Some(0);
    }
    if pages.len() < 2 && matched.is_none() {
        return String::new();
    }
    let mut line = String::from("<ul class=\"kp\">");
    for (i, (page, href)) in pages.iter().enumerate() {
        let label = escape_html(&truncate_chars(&page.label, 40));
        let label = if i == 0 && matched.is_some() {
            format!("<strong>{label}</strong>")
        } else {
            label
        };
        let _ = write!(
            line,
            "<li><a href=\"{}\" rel=\"noreferrer\">{label}</a></li>",
            escape_html(href)
        );
    }
    line.push_str("</ul>");
    line
}

/// "Wikipedia: Python (programming language)", under the result for the
/// site the page is about.
pub(crate) fn page_line(hit: &PageHit) -> String {
    let Some(href) = http_url(&hit.page.url) else {
        return String::new();
    };
    let description = hit
        .page
        .description
        .as_deref()
        .filter(|d| !d.trim().is_empty())
        .map(|d| format!(" &middot; {}", escape_html(d)))
        .unwrap_or_default();
    format!(
        "<p class=\"sub\">{}: <a href=\"{}\" rel=\"noreferrer\">{}</a>{description}</p>",
        escape_html(hit.page.set_name()),
        escape_html(&href),
        escape_html(&truncate_chars(&hit.page.title, 150)),
    )
}

pub(crate) fn render_hit(
    out: &mut String,
    hit: &Hit,
    network: Option<&NetworkResult>,
    go: Option<&str>,
    icon: Option<&str>,
    notes: &[String],
) {
    let name = hit
        .title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or(&hit.domain);
    let name = escape_html(&truncate_chars(name, 150));
    out.push_str(if network.is_some() {
        "<li class=\"net\">"
    } else {
        "<li>"
    });
    let badge = site_badge(&hit.domain, icon);
    let domain = escape_html(&hit.domain);
    match safe_href(hit) {
        Some(href) => {
            // The address, unless it says no more than the domain.
            let shown = display_url(&href);
            let url = if shown == hit.domain {
                String::new()
            } else {
                format!("<span class=\"u\">{}</span>", escape_html(&shown))
            };
            let _ = write!(
                out,
                "<a class=\"r\" href=\"{}\" rel=\"noreferrer\"><span class=\"site\">{badge}\
                 <span class=\"sn\"><span class=\"dn\">{domain}</span>{url}</span></span>\
                 <span class=\"t\">{name}</span></a>",
                escape_html(go.unwrap_or(&href)),
            );
        }
        None => {
            let _ = write!(
                out,
                "<div class=\"r\"><span class=\"site\">{badge}<span class=\"sn\">\
                 <span class=\"dn\">{domain}</span></span></span><span class=\"t\">{name}</span></div>"
            );
        }
    }
    if let Some(description) = hit.description.as_deref().filter(|d| !d.trim().is_empty()) {
        let _ = write!(out, "<p class=\"d\">{}</p>", escape_html(description));
    }
    // Already HTML.
    let mut meta: Vec<String> = notes.to_vec();
    if let Some(code) = hit.country.as_deref() {
        meta.push(escape_html(country_name(code)));
    }
    if let Some(result) = network {
        let crawl = if result.confirmed {
            "signed crawls, two nodes agree"
        } else if result.verified {
            "signed crawl, checked"
        } else {
            "unsigned (seed data)"
        };
        let answers = if result.answers == 1 {
            "1 answer".to_string()
        } else {
            format!("{} answers", result.answers)
        };
        meta.push(format!("from the Plumb network ({crawl}, in {answers})"));
    }
    let score = format!(
        "<span class=\"rank-meta\">{}<span title=\"text {:.3}, link {:.3}\">score {:.3}</span></span>",
        if meta.is_empty() { "" } else { " &middot; " },
        hit.text_score, hit.link_score, hit.score
    );
    let _ = writeln!(
        out,
        "<div class=\"m\">{}{score}</div></li>",
        meta.join(" &middot; ")
    );
}
