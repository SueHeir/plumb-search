//! A results page's list: this node's and the network's results merged,
//! the spelling note, the source line and the site search.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use plumb_core::{now_unix, truncate_chars, RecentNews};
use plumb_index::pages::{place_operator_pages, place_pages};
use plumb_index::{Hit, SearchOptions, SearchResults, SiteSearch, Spelling};

use super::api::{NetworkResult, NetworkResults};
use super::hit::{
    block_go_link, go_link, go_link_with, key_pages_line, page_line, product_of, render_hit,
    render_marked_page, render_recent,
};
use super::home::message_page;
use super::html::{
    escape_html, homepage_url, http_url, page, results_form, safe_href, settings_link, FOOTER,
};
use super::params::{NetSetting, Settings};
use super::plugin_results::{render_notes, render_offers, render_plugin};
use super::search::{NetOutcome, PLACES_MARK};
use super::{answers, history, Icons};
use crate::learn::{self, Block};
use crate::news::Recent;
use crate::websearch::Engine;

/// "Did you mean amazon?" above the results, which are for the query as
/// typed; or, when the results are for the spelling
/// ([`Spelling::applied`]), "Showing results for kanye west", with a link
/// to search for `typed` as typed.
fn render_spelling(out: &mut String, spelling: &Spelling, typed: &str, settings: &Settings) {
    let options = &settings.options;
    if spelling.applied {
        let as_typed = SearchOptions {
            exact: true,
            ..options.clone()
        };
        let _ = writeln!(
            out,
            "<p class=\"sp\">Showing results for <strong>{}</strong>. Search instead for \
             <a href=\"{}\">{}</a></p>",
            escape_html(&truncate_chars(&spelling.query, 150)),
            escape_html(&settings_link(
                "/search",
                typed,
                settings,
                &as_typed,
                settings.network == NetSetting::On
            )),
            escape_html(&truncate_chars(typed, 150))
        );
        return;
    }
    let fixed_options = SearchOptions {
        exact: false,
        ..options.clone()
    };
    let fixed = format!(
        "<a href=\"{}\"><strong>{}</strong></a>",
        escape_html(&settings_link(
            "/search",
            &spelling.query,
            settings,
            &fixed_options,
            settings.network == NetSetting::On
        )),
        escape_html(&truncate_chars(&spelling.query, 150))
    );
    // A site the corrected name is, marked as the suggestion's.
    let site = spelling
        .site
        .as_deref()
        .and_then(|domain| Some((domain, homepage_url(domain)?)))
        .map(|(domain, url)| {
            format!(
                " <a class=\"sps\" href=\"{}\" rel=\"noreferrer\">Go to {}</a>",
                escape_html(&url),
                escape_html(domain)
            )
        })
        .unwrap_or_default();
    let _ = writeln!(out, "<p class=\"sp\">Did you mean {fixed}?{site}</p>");
}

/// One result as shown: a hit, and what the network said about it when only
/// the network found it.
pub(crate) struct Shown<'a> {
    pub(crate) hit: Cow<'a, Hit>,
    pub(crate) network: Option<&'a NetworkResult>,
}

/// This node's hits and the network's, best score first, at most `limit`.
/// A site both found is shown once, as this node's: only sites this node
/// did not find count as from the network. Both lists are ranked with this
/// node's ranking and the same choices, so their scores compare, with one
/// exception: text scores are normalized within each list's own
/// candidates, and the network's are the few sites of a few buckets. When
/// none of those has all the query's main words, the best partial match
/// scores as if it had them: "Social Security in Spain", found by "in"
/// alone, came first for "weather in denver". So a site only the network
/// found that lacks some of the main words ([`Hit::missing_words`]) comes
/// after this node's hits, and shows only when this node has too few.
///
/// When the network holds a signed crawl of a site both found, that crawl
/// fills the title and description this node's copy lacks, and the site
/// keeps the better of the two scores: the network's text may match the
/// search where this node's copy, without text, barely does.
pub(crate) fn merge_results<'a>(
    local: &'a [Hit],
    network: &'a NetOutcome,
    limit: usize,
) -> Vec<Shown<'a>> {
    let signed: HashMap<&str, &NetworkResult> = match network {
        NetOutcome::Answered(results) => results
            .hits
            .iter()
            .filter(|result| result.shared.is_some())
            .map(|result| (result.hit.domain.as_str(), result))
            .collect(),
        _ => HashMap::new(),
    };
    let mut shown: Vec<Shown> = local
        .iter()
        .map(|hit| Shown {
            hit: match signed.get(hit.domain.as_str()) {
                Some(result) => Cow::Owned(fill_from_network(hit, result)),
                None => Cow::Borrowed(hit),
            },
            network: None,
        })
        .collect();
    if let NetOutcome::Answered(results) = network {
        let seen: HashSet<&str> = local.iter().map(|hit| hit.domain.as_str()).collect();
        let (partial, whole): (Vec<&NetworkResult>, Vec<&NetworkResult>) = results
            .hits
            .iter()
            .filter(|result| !seen.contains(result.hit.domain.as_str()))
            .partition(|result| result.hit.missing_words);
        let from_network = |result: &'a NetworkResult| Shown {
            hit: Cow::Borrowed(&result.hit),
            network: Some(result),
        };
        shown.extend(whole.into_iter().map(from_network));
        // Stable, so ties keep this node's hits first.
        shown.sort_by(|a, b| b.hit.score.total_cmp(&a.hit.score));
        shown.extend(partial.into_iter().map(from_network));
    }
    shown.truncate(limit);
    shown
}

/// This node's `hit` with what the network's signed crawl of the site adds:
/// the title and description it lacks, and the network's score when better
/// and the crawl has all the query's main words (see [`merge_results`]).
pub(crate) fn fill_from_network(hit: &Hit, result: &NetworkResult) -> Hit {
    let mut hit = hit.clone();
    let blank = |text: &Option<String>| text.as_deref().is_none_or(|t| t.trim().is_empty());
    if let Some(shared) = &result.shared {
        if blank(&hit.title) && !blank(&shared.title) {
            hit.title.clone_from(&shared.title);
        }
        if blank(&hit.description) && !blank(&shared.description) {
            hit.description.clone_from(&shared.description);
        }
        if hit.key_pages.is_empty() {
            hit.key_pages.clone_from(&shared.key_pages);
        }
    }
    if !result.hit.missing_words {
        hit.score = hit.score.max(result.hit.score);
    }
    hit
}

/// The line above the results that says where they came from.
pub(crate) fn render_source(
    out: &mut String,
    query: &str,
    settings: &Settings,
    network: &NetOutcome,
    from_network: usize,
) {
    let line = match (settings.network, network) {
        (NetSetting::Unavailable, _) => "From this site's own index.".to_string(),
        (_, NetOutcome::NotAsked) => format!(
            "From this site's own index. <a href=\"{}\">Use the Plumb network too</a>",
            escape_html(&settings_link(
                "/search",
                query,
                settings,
                &settings.options,
                true
            ))
        ),
        (_, NetOutcome::Failed) => {
            "From this site's own index: the Plumb network did not answer this time.".to_string()
        }
        (_, NetOutcome::Answered(results)) if results.pending > 0 => {
            let mut line = "From this site's index and any saved Plumb results. More results \
                            are waiting for the next background download; search again later."
                .to_string();
            if results.stale > 0 {
                line.push_str(" Some saved results may be out of date.");
            }
            if from_network > 0 {
                line.push_str(
                    " <span class=\"sw\"></span>Tinted results came from saved Plumb data.",
                );
            }
            line
        }
        (_, NetOutcome::Answered(results)) if results.asked == 0 && results.cached > 0 => {
            let mut line =
                "From this site's index and saved Plumb results, read locally.".to_string();
            if results.stale > 0 {
                line.push_str(
                    " Some saved results may be out of date and will refresh in the background.",
                );
            }
            if from_network > 0 {
                line.push_str(
                    " <span class=\"sw\"></span>Tinted results came only from the network.",
                );
            }
            line
        }
        (_, NetOutcome::Answered(results)) if results.asked == 0 => {
            "From this site's own index: none of the Plumb nodes it searches are connected right \
             now."
                .to_string()
        }
        (_, NetOutcome::Answered(results)) => {
            let mut line = format!(
                "From this site's index and the Plumb network: {} of {} requests to other nodes \
                 answered, without sending them your search.",
                results.answered, results.asked
            );
            if results.rejected > 0 {
                let _ = write!(
                    line,
                    " {} answers were dropped because their proofs did not check out.",
                    results.rejected
                );
            }
            if results.cached > 0 {
                line.push_str(" Some results came from saved Plumb data.");
            }
            if from_network > 0 {
                line.push_str(
                    " <span class=\"sw\"></span>Tinted results came only from the network.",
                );
            }
            line
        }
    };
    let _ = writeln!(out, "<p class=\"src\">{line}</p>");
}

/// [`render_results_with`] without a "Recent" block.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_results(
    query: &str,
    results: &SearchResults,
    extras: Option<&answers::Extras>,
    network: &NetOutcome,
    settings: &Settings,
    web_search: Option<Engine>,
    limit: usize,
    share_picks: bool,
    icons: &Icons,
) -> String {
    render_results_with(
        query,
        results,
        extras,
        network,
        settings,
        web_search,
        limit,
        share_picks,
        icons,
        None,
    )
}

/// [`render_results`] with a "Recent" block after the first result.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_results_with(
    query: &str,
    results: &SearchResults,
    extras: Option<&answers::Extras>,
    network: &NetOutcome,
    settings: &Settings,
    web_search: Option<Engine>,
    limit: usize,
    share_picks: bool,
    icons: &Icons,
    recent: Option<&Recent>,
) -> String {
    let notes = extras.map(|e| &e.plugin_notes);
    let notes_on = |url: Option<String>| -> &[crate::plugins::ResultNote] {
        url.and_then(|url| notes?.get(&url))
            .map(Vec::as_slice)
            .unwrap_or_default()
    };
    let hidden = |url: Option<String>| notes_on(url).iter().any(|note| note.hide);
    let mut shown = merge_results(&results.hits, network, limit);
    // Results a plugin of this node leaves out; other nodes' results are
    // not this node's to mark up.
    shown.retain(|item| item.network.is_some() || !hidden(safe_href(&item.hit)));
    let token = extras.and_then(|e| e.plugin_token.as_deref());
    let marked = |url: Option<String>| render_notes(notes_on(url), query, token);
    let from_network = shown.iter().filter(|s| s.network.is_some()).count();
    let mut body = format!(
        "<main{}>\n<h1 class=\"visually-hidden\">Results for {}</h1>",
        if settings.debug {
            " class=\"ranking-debug\""
        } else {
            ""
        },
        escape_html(query)
    );
    // Tuning: its bar comes first, above the places too.
    if let Some(bar) = settings.history.as_ref().and_then(|h| h.tune_bar.as_ref()) {
        body.push_str(bar);
        body.push_str(PLACES_MARK);
    }
    if let Some(profile) = extras.and_then(|e| e.profile.as_ref()) {
        let domain = plumb_core::registrable_domain(&profile.url).unwrap_or_default();
        answers::render_profile(&mut body, profile, icons.get(&domain));
    }
    if let Some(answer) = extras.and_then(|e| e.answer.as_ref()) {
        answers::render_answer(&mut body, answer);
    }
    render_source(&mut body, query, settings, network, from_network);
    if let Some(history) = &settings.history {
        // Edit mode: buttons to move results and fold boxes.
        let here = settings_link(
            "/search",
            query,
            settings,
            &settings.options,
            settings.network == NetSetting::On,
        );
        let line = if history.edit.is_some() {
            format!(
                "Edit mode: \u{25b2} higher, \u{25bc} lower, \u{2715} not for this search. Kept on \
                 this node for this browser. <a href=\"{}\">Done</a>",
                escape_html(&here)
            )
        } else {
            format!(
                "<a href=\"{}\">Edit these results</a> &middot; \
                 <a href=\"/tune\">Tune your search</a>",
                escape_html(&format!("{here}&edit=1"))
            )
        };
        if history.tune_bar.is_none() {
            let _ = writeln!(body, "<p class=\"s ed\">{line}</p>");
        }
    }
    if let Some(spelling) = &results.spelling {
        render_spelling(&mut body, spelling, query, settings);
    }
    // Picks are noted for the query: shared, or kept in the searcher's
    // history.
    let notes_picks = share_picks
        || settings.experiment.is_some()
        || settings
            .history
            .as_ref()
            .is_some_and(|history| history.prefs.on());
    if let Some(site_search) = &results.site_search {
        render_site_search(&mut body, site_search);
    }
    if let Some(engine) = web_search {
        let _ = writeln!(
            body,
            "<p class=\"web\"><a href=\"{}\" rel=\"noreferrer\">Search the web with {} for <strong>{}</strong></a></p>",
            escape_html(&engine.url(query)),
            escape_html(engine.name()),
            escape_html(&truncate_chars(query, 150))
        );
    }
    let shown_hits: Vec<Hit> = shown
        .iter()
        .map(|item| item.hit.clone().into_owned())
        .collect();
    let found_pages = results
        .pages
        .iter()
        .map(|placed| placed.hit.clone())
        .collect();
    let ops = plumb_core::Operators::parse(query);
    let mut pages = if ops.any() {
        place_operator_pages(&ops, &shown_hits, found_pages)
    } else {
        place_pages(query, &shown_hits, found_pages)
    };
    pages.retain(|placed| !hidden(http_url(&placed.hit.page.url)));
    // How much of each kind of page the searcher wants.
    if let Some(about) = settings.about() {
        about.apply_pages(&mut pages, shown_hits.len());
    }
    // An info box is about what the whole query names, which operators
    // ("site:", "-word") change, and a sum names nothing.
    let info = if let Some(profile) = extras.and_then(|e| e.profile.as_ref()) {
        answers::info_from_page(&profile.page, &shown_hits)
    } else if ops.any() || answers::is_sum(extras.and_then(|e| e.answer.as_ref())) {
        None
    } else {
        answers::info_box(&shown_hits, &pages)
    };
    let shown_count = shown.len();
    let pages = &pages;
    let listed_pages = move |at: usize| {
        pages.iter().filter(move |p| {
            p.under.is_none() && (p.at == at || (at == usize::MAX && p.at >= shown_count))
        })
    };
    // What was learned from the searcher's clicks: headlines they read for
    // searches like this come unfolded, and the box's links note a use.
    let learns = settings.history.as_ref().is_some_and(|h| h.learns);
    let news_view = match settings.options.recent {
        RecentNews::Collapsed if settings.history.as_ref().is_some_and(|h| h.open_news) => {
            RecentNews::Expanded
        }
        view => view,
    };
    let news_link = |href: &str| {
        if learns {
            block_go_link(query, &settings.options, Block::News, href)
        } else {
            href.to_owned()
        }
    };
    let news = recent
        .filter(|_| settings.options.recent != RecentNews::Off)
        .map(|recent| {
            let mut html = render_recent(recent, news_view, now_unix(), &news_link);
            if let Some(edit) = settings.history.as_ref().and_then(|h| h.edit.as_ref()) {
                let context = learn::news_context(recent.site.is_some());
                let now = settings.history.as_ref().and_then(|h| h.news_verdict);
                let buttons = history::box_buttons(edit, Block::News, context, now);
                if let Some(end) = html.rfind("</li>") {
                    html.insert_str(end, &buttons);
                }
            }
            html
        });
    let now = now_unix();
    let from_plugins: String = extras
        .map(|e| {
            let found: String = e
                .plugins
                .iter()
                .map(|p| render_plugin(p, now, query, e.plugin_token.as_deref()))
                .collect();
            found + &render_offers(&e.plugin_offers, query, &settings.options)
        })
        .unwrap_or_default();
    let news = match (news, from_plugins.is_empty()) {
        (news, true) => news,
        (news, false) => Some(news.unwrap_or_default() + &from_plugins),
    };
    if shown.is_empty() && pages.is_empty() {
        let _ = writeln!(
            body,
            "<p class=\"none\">No sites match <strong>{}</strong>.</p>",
            escape_html(query)
        );
        if let Some(news) = &news {
            let _ = writeln!(body, "<ol>\n{news}</ol>");
        }
    } else {
        body.push_str("<ol>\n");
        for (position, item) in shown.iter().enumerate() {
            for page in listed_pages(position) {
                render_marked_page(&mut body, &page.hit, icons, &marked);
            }
            // `/go` only follows this node's own results, so sites from other
            // nodes link straight to themselves.
            let go = (notes_picks && item.network.is_none()).then(|| match settings.experiment {
                // The page and the result's place, for the experiments.
                Some(token) => go_link_with(
                    query,
                    &settings.options,
                    &[
                        ("d", &item.hit.domain),
                        ("x", &token.to_string()),
                        ("p", &position.to_string()),
                    ],
                ),
                None => go_link(query, &settings.options, &item.hit.domain),
            });
            let icon = icons.get(&item.hit.domain);
            let notes = match (&settings.history, &settings.browser_about) {
                (Some(history), _) => history.notes(&item.hit),
                (None, Some(about)) => history::about_note(about, &item.hit).into_iter().collect(),
                (None, None) => Vec::new(),
            };
            let mut rendered = String::new();
            render_hit(
                &mut rendered,
                &item.hit,
                item.network,
                go.as_deref(),
                icon,
                &notes,
            );
            // The article about the site, and the site's own page the
            // query names.
            let carried: Vec<&plumb_index::pages::PlacedPage> = pages
                .iter()
                .filter(|p| p.under.as_deref() == Some(item.hit.domain.as_str()))
                .collect();
            for page in &carried {
                if let Some(end) = rendered.rfind("</li>") {
                    rendered.insert_str(end, &page_line(&page.hit));
                }
            }
            if item.network.is_none() {
                let line = marked(safe_href(&item.hit));
                if let Some(end) = rendered.rfind("</li>") {
                    rendered.insert_str(end, &line);
                }
            }
            let product = carried
                .iter()
                .find_map(|page| product_of(&page.hit, &item.hit.domain, query));
            if position == 0 || product.is_some() {
                if let Some(end) = rendered.rfind("<div class=\"m\">") {
                    let line = key_pages_line(&item.hit, query, product.as_ref());
                    rendered.insert_str(end, &line);
                }
            }
            body.push_str(&rendered);
            if position == 0 {
                if let Some(news) = &news {
                    body.push_str(news);
                }
            }
        }
        if shown.is_empty() {
            if let Some(news) = &news {
                body.push_str(news);
            }
        }
        for page in listed_pages(usize::MAX) {
            render_marked_page(&mut body, &page.hit, icons, &marked);
        }
        body.push_str("</ol>\n");
    }
    if share_picks {
        body.push_str(
            "<p class=\"s\">This node shares which result is opened for a search: it notes \
             the pick here and reports it to other Plumb nodes, encrypted so that no node can \
             read it until many nodes report the same pick, and never with who made it.</p>\n",
        );
    }
    let options = &settings.options;
    let api = escape_html(&settings_link(
        "/api/search",
        query,
        settings,
        options,
        false,
    ));
    let mut json = format!("<a href=\"{api}\">{api}</a>");
    if matches!(network, NetOutcome::Answered(_)) {
        let api = escape_html(&settings_link(
            "/api/network/search",
            query,
            settings,
            options,
            false,
        ));
        let _ = write!(json, " and <a href=\"{api}\">{api}</a>");
    }
    if settings.debug {
        let _ = write!(body, "<p class=\"s\">As JSON: {json}</p>");
    }
    let _ = write!(body, "{FOOTER}\n</main>\n");
    // Results keep the same left edge whether or not an info box shows; the
    // box sits beside them (above them on a narrow screen, unless a profile
    // asked for leads).
    let form = results_form(query, settings);
    let body = match &info {
        Some(info) => {
            let mut aside = String::new();
            answers::render_info_box(&mut aside, info);
            let cols = if extras.is_some_and(|e| e.profile.is_some()) {
                "cols pfirst"
            } else {
                "cols"
            };
            format!("<div class=\"wrap wide\">\n{form}\n<div class=\"{cols}\">\n{body}{aside}</div>\n</div>")
        }
        None => format!("<div class=\"wrap wide\">\n{form}\n{body}</div>"),
    };
    page(&format!("{query} - Plumb Search"), &body)
}

/// What other nodes answered, and nothing from this node. Their text is as
/// untrusted as any record's, and is escaped the same way.
pub(crate) fn render_network(
    query: &str,
    results: &NetworkResults,
    icons: &Icons,
    settings: &Settings,
) -> String {
    let mut body = format!(
        "<div class=\"wrap wide\">\n{}\n<main>\n",
        results_form(query, settings)
    );
    if results.asked == 0 && (results.cached > 0 || results.pending > 0) {
        body.push_str(
            "<p class=\"s\">Plumb results are read from saved data and ranked on this node.</p>\n",
        );
    } else {
        let _ = writeln!(
            body,
            "<p class=\"s\">From the Plumb network, without sending your search: {} buckets of \
         sites asked of other nodes under throwaway identities, {} of {} requests answered{}{}. \
         Ranked on this node.</p>",
            results.buckets,
            results.answered,
            results.asked,
            if results.direct > 0 {
                format!(
                    "; {} sent straight to a node, which saw this node's address, because no \
                 other node could pass them on",
                    results.direct
                )
            } else if results.asked > 0 {
                "; each through another node, so the node answering never saw this node's \
             address"
                    .to_string()
            } else {
                String::new()
            },
            if results.rejected > 0 {
                format!(
                    "; {} answers were dropped because their proofs did not check out",
                    results.rejected
                )
            } else {
                String::new()
            }
        );
    }
    if results.cached > 0 {
        let _ = writeln!(
            body,
            "<p class=\"s\">{} of this search's buckets came from saved Plumb data, so the \
             network was not asked for them again.</p>",
            results.cached
        );
    }
    if results.pending > 0 {
        body.push_str(
            "<p class=\"s\">More results are waiting for the next background download. \
             Search again later.</p>\n",
        );
    }
    if results.stale > 0 {
        body.push_str(
            "<p class=\"s\">Some saved results may be out of date and will refresh \
             in the background.</p>\n",
        );
    }
    if results.asked == 0 && results.cached == 0 && results.pending == 0 {
        body.push_str(
            "<p class=\"none\">None of the nodes this node searches are connected yet. It \
             keeps looking for them.</p>\n",
        );
    } else if results.hits.is_empty() && results.pending == 0 {
        let _ = writeln!(
            body,
            "<p class=\"none\">No node had a site matching <strong>{}</strong>.</p>",
            escape_html(query)
        );
    } else if !results.hits.is_empty() {
        body.push_str("<ol>\n");
        for result in &results.hits {
            let icon = icons.get(&result.hit.domain);
            render_hit(&mut body, &result.hit, Some(result), None, icon, &[]);
        }
        body.push_str("</ol>\n");
    }
    let local = escape_html(&settings_link(
        "/search",
        query,
        settings,
        &settings.options,
        settings.network == NetSetting::On,
    ));
    let _ = write!(
        body,
        "<p class=\"s\"><a href=\"{local}\">Back to this node's results</a></p>"
    );
    if settings.debug {
        let api = escape_html(&settings_link(
            "/api/network/search",
            query,
            settings,
            &settings.options,
            false,
        ));
        let _ = write!(
            body,
            "<p class=\"s\">As JSON: <a href=\"{api}\">{api}</a></p>"
        );
    }
    let _ = write!(body, "{FOOTER}</main></div>");
    page(&format!("{query} (network) - Plumb Search"), &body)
}

/// "Search github.com for sueheir plumb-search", above the results.
fn render_site_search(out: &mut String, site_search: &SiteSearch) {
    let Some(href) = http_url(&site_search.url) else {
        return;
    };
    let _ = writeln!(
        out,
        "<p class=\"ss\"><a href=\"{}\" rel=\"noreferrer\">Search {} for <strong>{}</strong></a></p>",
        escape_html(&href),
        escape_html(&site_search.domain),
        escape_html(&truncate_chars(&site_search.terms, 150))
    );
}

pub(crate) fn render_error(query: &str, settings: &Settings) -> String {
    message_page("Search could not finish", "Please try again in a moment. If you run this node, check Activity in the panel for details.", query, settings)
}
